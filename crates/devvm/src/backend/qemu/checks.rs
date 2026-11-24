//! The one line of `devvm doctor` that is about QEMU.

use super::args::ArchFiles;
use super::sidecar::{LazyInstall, configured};
use crate::backend::{Check, Detail};
use crate::store::fetch::hash_file;
use crate::{Arch, Result, Sha256};

#[cfg(test)]
#[path = "checks_tests.rs"]
mod checks_tests;

#[cfg(all(test, unix))]
#[path = "checks_unix_tests.rs"]
mod checks_unix_tests;

#[cfg(all(test, windows))]
#[path = "checks_windows_tests.rs"]
mod checks_windows_tests;

/// Whether QEMU can be had: the line that `doctor` prints and `availability` reports. It passes when the manifest has a
/// build for this target, which a launch downloads if it is not cached, or when the configured `qemu.path` is accepted
/// by the check a launch applies (`configured`). Only [`Detail::Full`] hashes the configured QEMU, which reads the
/// whole executable.
pub(super) fn qemu_check(install: &LazyInstall, detail: Detail) -> Check {
    Check {
        name: "qemu".to_owned(),
        outcome: qemu_outcome(install, detail).map_err(|e| e.message().to_owned()),
    }
}

fn qemu_outcome(install: &LazyInstall, detail: Detail) -> Result<String> {
    let Some(dir) = &install.config()?.qemu_path else {
        let entry = install.entry()?;
        return Ok(format!(
            "QEMU {}, zip sha256 {}, downloaded on first use if not cached",
            entry.qemu_version, entry.sha256
        ));
    };
    // Validated on every call, never cached.
    let qemu = configured(dir, Arch::host())?;
    match detail {
        Detail::Brief => Ok(format!("QEMU in {}", qemu.bin.display())),
        Detail::Full => {
            let system = ArchFiles::of(Arch::host()).system;
            // Informational.
            let hash = match hash_file(&qemu.tool(&system)) {
                Ok(bytes) => Sha256::from_bytes(bytes).to_string(),
                Err(e) => format!("unavailable ({})", e.message()),
            };
            Ok(format!("QEMU in {}, {system} sha256 {hash}", qemu.bin.display()))
        }
    }
}
