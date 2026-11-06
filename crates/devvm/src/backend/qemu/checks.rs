//! The one line of `devvm doctor` that is about QEMU.

use super::sidecar::{CONFIGURED_SYSTEM, LazyInstall};
use crate::backend::{Check, Detail};
use crate::store::fetch::hash_file;
use crate::{Error, ErrorKind, Result, Sha256};

#[cfg(test)]
#[path = "checks_tests.rs"]
mod checks_tests;

/// Whether QEMU can be had: the line that `doctor` prints and `availability` reports. It passes when the manifest has a
/// build for this target, which a launch downloads if it is not cached, or when the configured `qemu.path` is accepted
/// by the check a launch applies (`LazyInstall::existing`). Nothing is downloaded, and no cache is looked at. Only
/// [`Detail::Full`] hashes the configured QEMU, which reads the whole executable.
pub(super) fn qemu_check(install: &LazyInstall, detail: Detail) -> Check {
    Check {
        name: "qemu".to_owned(),
        outcome: qemu_outcome(install, detail).map_err(|e| e.message().to_owned()),
    }
}

fn qemu_outcome(install: &LazyInstall, detail: Detail) -> Result<String> {
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
    match detail {
        Detail::Brief => Ok(format!("QEMU in {}", qemu.bin.display())),
        Detail::Full => {
            let hash = Sha256::from_bytes(hash_file(&qemu.tool(CONFIGURED_SYSTEM))?);
            Ok(format!(
                "QEMU in {}, {CONFIGURED_SYSTEM} sha256 {hash}",
                qemu.bin.display()
            ))
        }
    }
}
