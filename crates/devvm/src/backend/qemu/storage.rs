//! qcow2 storage: a layer is a `disk.qcow2` (and the firmware variables `vars.fd`); an instance disk is a copy-on-write
//! overlay of the tip layer. Everything goes through the sidecar's `qemu-img`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::directory_of;
use super::sidecar::LazyInstall;
use crate::backend::{DEFAULT_DISK, Storage};
use crate::host::Host;
use crate::store::layers::LayerDir;
use crate::{Arch, Error, ErrorKind, Result};

#[cfg(test)]
#[path = "storage_tests.rs"]
mod storage_tests;

/// The disk file of a layer and of an instance's staging.
pub(crate) const DISK: &str = "disk.qcow2";
/// A layer's UEFI variables, and the copy each instance boots from.
pub(crate) const VARS: &str = "vars.fd";
/// The format of every layer and instance disk.
pub(crate) const QCOW2: &str = "qcow2";
/// The largest virtual size of an instance disk: 2 PiB, the most `qemu-img create -f qcow2` makes with its default
/// 64 KiB clusters. An L2 table maps 512 MiB and the L1 table holds at most `QCOW_MAX_L1_SIZE` / `L1E_SIZE` = 4 Mi
/// tables (QEMU 11.1.2 `block/qcow2.h:59,104`). Past that `qcow2_co_create` (`block/qcow2.c:3918`) fails in
/// `qcow2_co_truncate`, which calls `qcow2_grow_l1_table` (`block/qcow2.c:4563`; `-EFBIG` at `block/qcow2-cluster.c:120`).
const QCOW2_MAX_SIZE: u64 = 1 << 51;

pub(crate) struct QemuStorage {
    install: Arc<LazyInstall>,
    host: &'static dyn Host,
}

impl QemuStorage {
    pub(crate) fn new(install: Arc<LazyInstall>, host: &'static dyn Host) -> QemuStorage {
        QemuStorage { install, host }
    }

    /// Runs `qemu-img` with `args` in QEMU's own environment, windowless. Its stdout on success; an error carrying its
    /// stderr otherwise.
    fn qemu_img(&self, args: &[&OsStr]) -> Result<Vec<u8>> {
        let install = self.install.get()?;
        let tool = install.tool("qemu-img");
        let mut command = install.command("qemu-img", self.host)?;
        command.args(args);
        let shown = || {
            args.iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let output = command
            .output()
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot run {} {}", tool.display(), shown()), e))?;
        if !output.status.success() {
            return Err(Error::new(
                ErrorKind::Io,
                format!(
                    "qemu-img {} failed ({}): {}",
                    shown(),
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        Ok(output.stdout)
    }

    /// `qemu-img info` of `image` as JSON. With `format`, the file is read as that format and never probed.
    fn info(&self, image: &Path, format: Option<&str>) -> Result<serde_json::Value> {
        let mut args: Vec<&OsStr> = vec!["info".as_ref(), "--output=json".as_ref()];
        if let Some(format) = format {
            args.extend([OsStr::new("-f"), OsStr::new(format)]);
        }
        args.push(image.as_os_str());
        let stdout = self.qemu_img(&args)?;
        serde_json::from_slice(&stdout).map_err(|e| {
            Error::new(
                ErrorKind::Internal,
                format!("qemu-img info {} did not print JSON: {e}", image.display()),
            )
        })
    }

    /// The size in bytes of the qcow2 disk `image` as a guest sees it.
    fn virtual_size(&self, image: &Path) -> Result<u64> {
        virtual_size_in(&self.info(image, Some(QCOW2))?, image)
    }

    /// The directory holding the firmware variables templates.
    fn share(&self) -> Result<PathBuf> {
        Ok(self.install.get()?.share.clone())
    }
}

/// The `virtual-size` in `info`, the output of `qemu-img info` of `image`.
fn virtual_size_in(info: &serde_json::Value, image: &Path) -> Result<u64> {
    info["virtual-size"].as_u64().ok_or_else(|| {
        Error::new(
            ErrorKind::Internal,
            format!("qemu-img info {} did not print a virtual-size", image.display()),
        )
    })
}

/// The size of the disk `qemu-img create` makes of a request for `size` bytes: rounded up to 512 bytes. `Recipe` when
/// that is more than a qcow2 disk can have.
fn made_size(size: u64) -> Result<u64> {
    size.checked_next_multiple_of(512)
        .filter(|made| *made <= QCOW2_MAX_SIZE)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Recipe,
                format!(
                    "a disk of {size} bytes is more than the {QCOW2_MAX_SIZE} bytes a qcow2 disk can have; \
                     set resources.disk to at most {QCOW2_MAX_SIZE}"
                ),
            )
        })
}

fn copy_file(from: &Path, to: &Path) -> Result<()> {
    std::fs::copy(from, to).map(drop).map_err(|e| {
        Error::io(
            ErrorKind::Io,
            format!("cannot copy {} to {}", from.display(), to.display()),
            e,
        )
    })
}

/// `dir/vars.fd`, which every machine boots with: `Store` naming it when `dir` has none. A copy that skipped it would
/// make a machine QEMU refuses to start, found only at its first boot.
fn require_vars(dir: &Path, what: &str) -> Result<PathBuf> {
    let vars = dir.join(VARS);
    if vars.is_file() {
        return Ok(vars);
    }
    Err(Error::new(
        ErrorKind::Store,
        format!(
            "{what} {} has no {VARS}, the firmware variables a machine needs to boot",
            dir.display()
        ),
    ))
}

impl Storage for QemuStorage {
    fn create_instance_disk(&self, layer: &LayerDir, into: &Path, size: Option<u64>) -> Result<()> {
        let made = size.map(made_size).transpose()?;
        let vars = require_vars(layer.path(), "the layer")?;
        let backing = layer.path().join(DISK);
        let beside = directory_of(into, "the instance disk")?.join(VARS);
        let layer_size = self.virtual_size(&backing)?;
        if let (Some(size), Some(made)) = (size, made)
            && made < layer_size
        {
            return Err(Error::new(
                ErrorKind::Recipe,
                format!(
                    "a disk of {size} bytes is smaller than the {layer_size} bytes of the layer {}; \
                     set resources.disk to at least {layer_size}",
                    layer.path().display()
                ),
            ));
        }
        // A layer imported before `import_base` checked its size can be over the ceiling, and no request was refused for
        // it above.
        if size.is_none() && layer_size > QCOW2_MAX_SIZE {
            return Err(Error::new(
                ErrorKind::Recipe,
                format!(
                    "the layer {} is {layer_size} bytes, more than the {QCOW2_MAX_SIZE} bytes a disk can have",
                    layer.path().display()
                ),
            ));
        }
        let size = size.unwrap_or(DEFAULT_DISK.max(layer_size));
        self.qemu_img(&[
            "create".as_ref(),
            "-f".as_ref(),
            QCOW2.as_ref(),
            "-b".as_ref(),
            backing.as_os_str(),
            "-F".as_ref(),
            QCOW2.as_ref(),
            into.as_os_str(),
            size.to_string().as_ref(),
        ])?;
        copy_file(&vars, &beside)
    }

    /// The layer is a copy of the instance disk, which is only read. Its backing file stays the absolute path the
    /// instance disk was created over: a relative path would be resolved against the child's path as written, so each
    /// level of the chain would add `../../<key>/<generation>/` to it, and a chain of four layers outgrows Windows'
    /// 260-character path limit, which QEMU does not lift.
    fn commit_layer(&self, instance_disk: &Path, staging: &LayerDir) -> Result<()> {
        let vars = require_vars(
            directory_of(instance_disk, "the instance disk")?,
            "the instance directory",
        )?;
        let info = self.info(instance_disk, Some(QCOW2))?;
        if info.get("full-backing-filename").is_none() {
            return Err(Error::new(
                ErrorKind::Internal,
                format!(
                    "the instance disk {} has no backing file to commit onto",
                    instance_disk.display()
                ),
            ));
        }
        copy_file(instance_disk, &staging.path().join(DISK))?;
        copy_file(&vars, &staging.path().join(VARS))
    }

    fn import_base(&self, image: &Path, arch: Arch, staging: &LayerDir) -> Result<()> {
        let info = self.info(image, None)?;
        // An instance disk covers the base, and the most it can cover is `QCOW2_MAX_SIZE`.
        let size = virtual_size_in(&info, image)?;
        if size > QCOW2_MAX_SIZE {
            return Err(Error::new(
                ErrorKind::Recipe,
                format!(
                    "the base image {} is {size} bytes, more than the {QCOW2_MAX_SIZE} bytes a disk can have",
                    image.display()
                ),
            ));
        }
        let disk = staging.path().join(DISK);
        if info["format"] == QCOW2 {
            if info.get("backing-filename").is_some() {
                return Err(Error::new(
                    ErrorKind::Unavailable,
                    format!(
                        "the base image {} has a backing file, so it is not self-contained",
                        image.display()
                    ),
                ));
            }
            copy_file(image, &disk)?;
        } else {
            self.qemu_img(&[
                "convert".as_ref(),
                "-O".as_ref(),
                QCOW2.as_ref(),
                image.as_os_str(),
                disk.as_os_str(),
            ])?;
        }
        let template = match arch {
            Arch::Amd64 => "edk2-i386-vars.fd",
            Arch::Arm64 => "edk2-arm-vars.fd",
        };
        copy_file(&self.share()?.join(template), &staging.path().join(VARS))
    }
}
