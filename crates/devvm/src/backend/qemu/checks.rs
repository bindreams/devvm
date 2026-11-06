//! The one line of `devvm doctor` that is about QEMU.

use super::args::qemu_arch;
use super::sidecar::LazyInstall;
use crate::backend::Check;
use crate::store::fetch::hash_file;
use crate::{Arch, Error, ErrorKind, Result, Sha256};

#[cfg(test)]
#[path = "checks_tests.rs"]
mod checks_tests;

/// Whether QEMU can be had: the line that `doctor` prints and `availability` reports. It passes when the manifest has a
/// build for this target, which a launch downloads if it is not cached, or when the configured `qemu.path` is accepted
/// by the check a launch applies (`LazyInstall::existing`). Nothing is downloaded, and no cache is looked at.
pub(super) fn qemu_check(install: &LazyInstall) -> Check {
    Check {
        name: "qemu".to_owned(),
        outcome: qemu_outcome(install).map_err(|e| e.message().to_owned()),
    }
}

fn qemu_outcome(install: &LazyInstall) -> Result<String> {
    if install.config()?.qemu_path.is_none() {
        let entry = install.entry()?;
        return Ok(format!(
            "QEMU {}, zip sha256 {}, downloaded on first use if not cached",
            entry.qemu_version, entry.sha256
        ));
    }
    let qemu = install
        .existing()?
        .ok_or_else(|| Error::new(ErrorKind::Internal, "a configured qemu.path gave no install".to_owned()))?;
    let name = format!("qemu-system-{}", qemu_arch(Arch::host()));
    let hash = Sha256::from_bytes(hash_file(&qemu.tool(&name))?);
    Ok(format!("QEMU in {}, {name} sha256 {hash}", qemu.bin.display()))
}
