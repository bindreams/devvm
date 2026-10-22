//! OS file locks: the only exclusion primitive in devvm. The OS releases them when the holder dies.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use crate::exit::Progress;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "lock_tests.rs"]
pub(crate) mod lock_tests;

/// Whether a lock admits other holders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Shared,
    Exclusive,
}

/// A lock file and, while taken, its lock (`flock` / `LockFileEx`). Dropping the value releases the lock.
///
/// A `FileLock` is locked at most once at a time: locking a held value would convert the lock (`flock`) or deadlock
/// (`LockFileEx`), so it is a debug assertion failure. Take another `FileLock::open` of the same path for a second
/// holder. Locking and unlocking take `&mut self`, so two threads cannot share one value and both believe they hold
/// it. Lock files hold no data.
#[derive(Debug)]
pub struct FileLock {
    path: PathBuf,
    file: File,
    held: bool,
}

impl FileLock {
    /// Opens the lock file, creating it and its parent directories.
    pub fn open(path: &Path) -> Result<FileLock> {
        let fail = |source| {
            Error::io(
                ErrorKind::Io,
                format!("cannot open lock file {}", path.display()),
                source,
            )
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(fail)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(fail)?;
        Ok(FileLock {
            path: path.to_owned(),
            file,
            held: false,
        })
    }

    /// Takes the lock if nobody else holds a conflicting one. `Ok(false)` means someone else does.
    pub fn try_lock(&mut self, mode: Mode) -> Result<bool> {
        debug_assert!(!self.held, "{} is already locked", self.path.display());
        let result = match mode {
            Mode::Shared => self.file.try_lock_shared(),
            Mode::Exclusive => self.file.try_lock(),
        };
        match result {
            Ok(()) => {
                self.held = true;
                Ok(true)
            }
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(source)) => Err(self.failure("lock", source)),
        }
    }

    /// Takes the lock, waiting for it with no time limit. On contention it first says `waiting for <what>`.
    pub fn lock(&mut self, mode: Mode, what: &str, progress: &dyn Progress) -> Result<()> {
        if self.try_lock(mode)? {
            return Ok(());
        }
        progress.message(&format!("waiting for {what}"));
        let result = retry_interrupted(|| match mode {
            Mode::Shared => self.file.lock_shared(),
            Mode::Exclusive => self.file.lock(),
        });
        result.map_err(|source| self.failure("lock", source))?;
        self.held = true;
        Ok(())
    }

    /// Releases the lock; the value can be locked again.
    pub fn unlock(&mut self) -> Result<()> {
        self.file.unlock().map_err(|source| self.failure("unlock", source))?;
        self.held = false;
        Ok(())
    }

    /// The open file, for handle inheritance by a child process.
    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn failure(&self, action: &str, source: std::io::Error) -> Error {
        Error::io(
            ErrorKind::Io,
            format!("cannot {action} {}", self.path.display()),
            source,
        )
    }
}

/// Runs a blocking call again for as long as a signal interrupts it (`EINTR`); no cap, since the call only fails that
/// way without having done anything.
fn retry_interrupted<T>(mut call: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    loop {
        match call() {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}
