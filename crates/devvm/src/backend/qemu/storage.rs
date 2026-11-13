//! qcow2 storage: a layer is a `disk.qcow2` (and the firmware variables `vars.fd`); an instance disk is a copy-on-write
//! overlay of the tip layer. Everything goes through the sidecar's `qemu-img`.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::args::{ArchFiles, escape_option_value};
use super::sidecar::LazyInstall;
use crate::backend::{
    DEFAULT_DISK, Patch, QCOW2_MAX_SIZE, Storage, check_blank_disk_size, check_overlay, directory_of, made_size,
    make_overlay,
};
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

/// `Recipe` when no qcow2 disk is `size` bytes: the size of an instance disk is the recipe's `resources.disk`.
fn instance_disk_size(size: u64) -> Result<u64> {
    made_size(size).ok_or_else(|| {
        Error::new(
            ErrorKind::Recipe,
            format!(
                "a disk of {size} bytes is more than the {QCOW2_MAX_SIZE} bytes a qcow2 disk can have; set \
                 resources.disk to at most {QCOW2_MAX_SIZE}"
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
        let made = size.map(instance_disk_size).transpose()?;
        let vars = require_vars(layer.path(), "the layer")?;
        let backing = layer.path().join(DISK);
        let beside = directory_of(into, "the instance disk")?.join(VARS);
        let layer_size = self.virtual_size(&backing)?;
        if layer_size > QCOW2_MAX_SIZE {
            return Err(Error::new(
                ErrorKind::Store,
                format!(
                    "the layer {} is {layer_size} bytes, more than the {QCOW2_MAX_SIZE} bytes a disk can have; \
                     build the recipe from a base within that limit",
                    layer.path().display()
                ),
            ));
        }
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
        self.add_vars(arch, staging)
    }

    fn create_blank_disk(&self, size: u64, arch: Arch, staging: &LayerDir) -> Result<()> {
        check_blank_disk_size(size)?;
        self.qemu_img(&[
            "create".as_ref(),
            "-f".as_ref(),
            QCOW2.as_ref(),
            staging.path().join(DISK).as_os_str(),
            size.to_string().as_ref(),
        ])?;
        self.add_vars(arch, staging)
    }

    /// `qemu-img create` of the overlay, then one `qemu-img convert -n` per patch: the raw patch file is written into the
    /// overlay as the sub-range `[offset, offset + len)` of the disk, so `backing` is never written. [`check_overlay`]
    /// runs first. The overlay is made at a new path beside `into` and published by [`make_overlay`].
    fn overlay_with_patches(&self, backing: &Path, patches: &[Patch], into: &Path) -> Result<()> {
        check_overlay(backing, patches, into)?;
        let directory = directory_of(into, "the overlay")?;
        make_overlay(into, |staging| {
            let target = escape_option_value(staging)?;
            self.qemu_img(&[
                "create".as_ref(),
                "-f".as_ref(),
                QCOW2.as_ref(),
                "-b".as_ref(),
                backing.as_os_str(),
                "-F".as_ref(),
                "raw".as_ref(),
                staging.as_os_str(),
            ])?;
            patches
                .iter()
                .try_for_each(|patch| self.write_patch(patch, &target, directory))
        })
    }
}

impl QemuStorage {
    /// Copies the firmware variables template for `arch` into `staging`.
    fn add_vars(&self, arch: Arch, staging: &LayerDir) -> Result<()> {
        copy_file(
            &self.share()?.join(ArchFiles::of(arch).vars),
            &staging.path().join(VARS),
        )
    }

    /// Writes `patch` into the qcow2 disk at `target` (an option value), through a raw file in `directory` that is gone
    /// afterwards.
    fn write_patch(&self, patch: &Patch, target: &str, directory: &Path) -> Result<()> {
        let io = |e| Error::io(ErrorKind::Io, "cannot write a patch file", e);
        let mut file = tempfile::Builder::new()
            .prefix("patch-")
            .tempfile_in(directory)
            .map_err(io)?;
        file.write_all(&patch.bytes).map_err(io)?;
        file.flush().map_err(io)?;
        // The handle is closed: `qemu-img` opens the file by name, which Windows lets a process do only once the
        // other handles are gone.
        let source = file.into_temp_path();
        self.qemu_img(&[
            "convert".as_ref(),
            "-n".as_ref(),
            "-f".as_ref(),
            "raw".as_ref(),
            "--target-image-opts".as_ref(),
            source.as_os_str(),
            format!(
                "driver=raw,offset={},size={},file.driver=qcow2,file.file.driver=file,file.file.filename={target}",
                patch.offset,
                patch.bytes.len()
            )
            .as_ref(),
        ])?;
        Ok(())
    }
}
