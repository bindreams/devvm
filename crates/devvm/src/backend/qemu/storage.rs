//! qcow2 storage: a layer is a `disk.qcow2` (and the firmware variables `vars.fd`); an instance disk is a copy-on-write
//! overlay of the tip layer. Everything goes through the sidecar's `qemu-img`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::directory_of;
use super::sidecar::LazyInstall;
use crate::backend::Storage;
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

    /// `qemu-img info` of `image` as JSON.
    fn info(&self, image: &Path) -> Result<serde_json::Value> {
        let stdout = self.qemu_img(&["info".as_ref(), "--output=json".as_ref(), image.as_os_str()])?;
        serde_json::from_slice(&stdout).map_err(|e| {
            Error::new(
                ErrorKind::Internal,
                format!("qemu-img info {} did not print JSON: {e}", image.display()),
            )
        })
    }

    /// The directory holding the firmware variables templates.
    fn share(&self) -> Result<PathBuf> {
        Ok(self.install.get()?.share.clone())
    }
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
    fn create_instance_disk(&self, layer: &LayerDir, into: &Path, size: u64) -> Result<()> {
        let vars = require_vars(layer.path(), "the layer")?;
        let backing = layer.path().join(DISK);
        let beside = directory_of(into, "the instance disk")?.join(VARS);
        self.qemu_img(&[
            "create".as_ref(),
            "-f".as_ref(),
            "qcow2".as_ref(),
            "-b".as_ref(),
            backing.as_os_str(),
            "-F".as_ref(),
            "qcow2".as_ref(),
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
        let info = self.info(instance_disk)?;
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
        let info = self.info(image)?;
        let disk = staging.path().join(DISK);
        if info["format"] == "qcow2" {
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
                "qcow2".as_ref(),
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
