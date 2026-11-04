//! OS file locks: the only exclusion primitive in devvm. The OS releases them when the holder dies.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use crate::exit::Progress;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "lock_tests.rs"]
pub(crate) mod lock_tests;

#[cfg(all(test, unix))]
#[path = "lock_unix_tests.rs"]
mod lock_unix_tests;

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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| {
                Error::io(
                    ErrorKind::Io,
                    format!("cannot open lock file {}", path.display()),
                    source,
                )
            })?;
        }
        FileLock::open_in_existing_dir(path)
    }

    /// Opens the lock file, creating it but no directory: for a directory that someone else makes, with permissions of
    /// their own. A missing directory is an error that names it.
    pub fn open_in_existing_dir(path: &Path) -> Result<FileLock> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|source| {
                let text = match path.parent() {
                    // A dangling symlink at `path` also gives NotFound, and then the parent may be there.
                    Some(parent)
                        if source.kind() == std::io::ErrorKind::NotFound
                            && matches!(crate::host::symlink_metadata_if_present(parent), Ok(None)) =>
                    {
                        format!(
                            "cannot open lock file {}: {} does not exist",
                            path.display(),
                            parent.display()
                        )
                    }
                    _ => format!("cannot open lock file {}", path.display()),
                };
                Error::io(ErrorKind::Io, text, source)
            })?;
        Ok(FileLock {
            path: path.to_owned(),
            file,
            held: false,
        })
    }

    /// Opens an existing lock file without creating it or its directories; `None` if it is absent.
    pub fn open_existing(path: &Path) -> Result<Option<FileLock>> {
        match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => Ok(Some(FileLock {
                path: path.to_owned(),
                file,
                held: false,
            })),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::io(
                ErrorKind::Io,
                format!("cannot open lock file {}", path.display()),
                source,
            )),
        }
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
        let locked = self.lock_while(mode, what, progress, None)?;
        debug_assert!(locked, "a wait with no pause ends only with the lock");
        Ok(())
    }

    /// [`FileLock::lock`] that can be given up on. With `pause`, the lock is tried again after each call of it, and
    /// `Ok(true)` from it ends the wait: this returns `Ok(false)`, the lock not taken. A `pause` blocks for a moment or
    /// until something it watches changes. Without `pause`, the wait is the OS's and ends only with the lock.
    pub fn lock_while(
        &mut self,
        mode: Mode,
        what: &str,
        progress: &dyn Progress,
        pause: Option<&mut dyn FnMut() -> Result<bool>>,
    ) -> Result<bool> {
        if self.try_lock(mode)? {
            return Ok(true);
        }
        progress.message(&format!("waiting for {what}"));
        let Some(pause) = pause else {
            let result = retry_interrupted(|| match mode {
                Mode::Shared => self.file.lock_shared(),
                Mode::Exclusive => self.file.lock(),
            });
            result.map_err(|source| self.failure("lock", source))?;
            self.held = true;
            return Ok(true);
        };
        loop {
            if pause()? {
                return Ok(false);
            }
            if self.try_lock(mode)? {
                return Ok(true);
            }
        }
    }

    /// Releases the lock; the value can be locked again.
    pub fn unlock(&mut self) -> Result<()> {
        self.file.unlock().map_err(|source| self.failure("unlock", source))?;
        self.held = false;
        Ok(())
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
