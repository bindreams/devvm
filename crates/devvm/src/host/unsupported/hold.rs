//! A hold off Windows: an exclusive `flock`, which belongs to the open file and so to every duplicate of it, an
//! inherited descriptor included.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use crate::host::{Hold, HoldOutcome};

#[derive(Debug)]
struct FlockHold {
    file: File,
    path: PathBuf,
}

/// Creates `path` with `options` (private to the user) and holds it.
pub(super) fn new(mut options: OpenOptions, path: &Path) -> std::io::Result<Box<dyn Hold>> {
    let file = options.read(true).write(true).create_new(true).open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Box::new(FlockHold {
            file,
            path: path.to_owned(),
        })),
        Err(TryLockError::WouldBlock) => {
            drop(file);
            let _ = std::fs::remove_file(path);
            Err(std::io::Error::other(format!(
                "{} was locked by another process as it was created",
                path.display()
            )))
        }
        Err(TryLockError::Error(e)) => {
            drop(file);
            let _ = std::fs::remove_file(path);
            Err(e)
        }
    }
}

pub(super) fn existing(path: &Path) -> std::io::Result<HoldOutcome> {
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HoldOutcome::Absent),
        Err(e) => return Err(e),
    };
    match file.try_lock() {
        Ok(()) => Ok(HoldOutcome::Taken(Box::new(FlockHold {
            file,
            path: path.to_owned(),
        }))),
        Err(TryLockError::WouldBlock) => Ok(HoldOutcome::Busy),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

impl Hold for FlockHold {
    fn file(&self) -> &File {
        &self.file
    }

    /// Unlinks the name while the lock is still held, so nobody holds a file that has another's name.
    fn delete(self: Box<Self>) -> std::io::Result<()> {
        std::fs::remove_file(&self.path)
    }
}
