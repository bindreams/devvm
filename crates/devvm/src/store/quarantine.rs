//! Moving what cannot be read to quarantine, which keeps it: a file or directory that devvm cannot make sense of is
//! never deleted.

use std::path::{Path, PathBuf};

use crate::exit::Progress;
use crate::{Error, ErrorKind, Result, random};

#[cfg(test)]
#[path = "quarantine_tests.rs"]
mod quarantine_tests;

/// A fresh, not yet existing path `<root>/<label>-<random>`; `root` exists.
pub(crate) fn quarantine_path(root: &Path, label: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(root)
        .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", root.display()), e))?;
    Ok(root.join(format!("{label}-{}", hex::encode(random::bytes::<16>()))))
}

/// Moves the unreadable `file` into a fresh directory under `root`, which keeps its name, and says so. Never deletes
/// it. `None` when `file` is not there: whoever else handled it got there first.
pub(crate) fn quarantine_file(
    root: &Path,
    label: &str,
    file: &Path,
    progress: &dyn Progress,
) -> Result<Option<PathBuf>> {
    let dir = quarantine_path(root, label)?;
    std::fs::create_dir(&dir).map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", dir.display()), e))?;
    let target = dir.join(file.file_name().expect("a quarantined file has a name"));
    match std::fs::rename(file, &target) {
        Ok(()) => {}
        Err(e) if crate::host::is_absent(&e) => {
            crate::host::remove_tree(&dir)
                .map_err(|e| Error::io(ErrorKind::Io, format!("cannot remove {}", dir.display()), e))?;
            return Ok(None);
        }
        Err(e) => {
            return Err(Error::io(
                ErrorKind::Io,
                format!("cannot move {} to {}", file.display(), target.display()),
                e,
            ));
        }
    }
    progress.message(&format!(
        "moved the unreadable {} to {}",
        file.display(),
        target.display()
    ));
    Ok(Some(target))
}
