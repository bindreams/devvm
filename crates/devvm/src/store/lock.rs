//! OS file locks: the only exclusion primitive in devvm. The OS releases them when the holder dies.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

use crate::exit::Progress;
use crate::host::{self, Comparison, Host};
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
pub struct FileLock {
    path: PathBuf,
    file: File,
    held: bool,
    /// The host that opened the file, which answers every look at it.
    host: &'static dyn Host,
}

impl std::fmt::Debug for FileLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileLock")
            .field("path", &self.path)
            .field("file", &self.file)
            .field("held", &self.held)
            .finish_non_exhaustive()
    }
}

impl FileLock {
    /// Opens the lock file, creating it and its parent directories.
    pub fn open(path: &Path) -> Result<FileLock> {
        FileLock::create_with_host(host::current(), path)
    }

    /// Opens the lock file, creating it but no directory: for a directory that someone else makes, with permissions of
    /// their own. A missing directory is an error that names it.
    pub fn open_in_existing_dir(path: &Path) -> Result<FileLock> {
        FileLock::create_in(host::current(), path)
    }

    /// Opens an existing lock file without creating it or its directories; `None` if it is absent
    /// ([`crate::host::is_absent`]).
    pub fn open_existing(path: &Path) -> Result<Option<FileLock>> {
        FileLock::open_existing_with_host(host::current(), path)
    }

    /// [`FileLock::open`] through `host`.
    pub(crate) fn create_with_host(host: &'static dyn Host, path: &Path) -> Result<FileLock> {
        make_parents(path)?;
        FileLock::create_in(host, path)
    }

    fn create_in(host: &'static dyn Host, path: &Path) -> Result<FileLock> {
        let file = host.open_lock_file(path, true).map_err(|source| {
            // The file is made if it is missing, so only its directory can be; a link at the directory is there, and
            // leads nowhere.
            let text = match path.parent() {
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
            host,
        })
    }

    /// [`FileLock::open_existing`] through `host`.
    pub(crate) fn open_existing_with_host(host: &'static dyn Host, path: &Path) -> Result<Option<FileLock>> {
        match host.open_lock_file(path, false) {
            Ok(file) => Ok(Some(FileLock {
                path: path.to_owned(),
                file,
                held: false,
                host,
            })),
            Err(source) if crate::host::is_absent(&source) => Ok(None),
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

    /// How the file at [`FileLock::path`] compares with the file this lock is on; see [`Comparison`]. A lookup that
    /// fails is an error, never `Absent`; the caller keeps unlinkers away for the look (see `Locks`).
    pub(crate) fn is_at_path(&self) -> Result<Comparison> {
        let held = self
            .host
            .file_id_of_open(&self.file)
            .map_err(|e| self.failure("look at", e))?;
        held.compare(self.host.file_id(&self.path))
            .map_err(|e| self.failure("look at", e))
    }

    /// [`Host::volume_has_posix_unlink_rename`] for this file.
    pub(crate) fn volume_has_posix_unlink_rename(&self) -> Result<bool> {
        self.host
            .volume_has_posix_unlink_rename(&self.file)
            .map_err(|e| self.failure("look at the volume of", e))
    }

    fn failure(&self, action: &str, source: std::io::Error) -> Error {
        Error::io(
            ErrorKind::Io,
            format!("cannot {action} {}", self.path.display()),
            source,
        )
    }
}

/// Makes the directories above the lock file `path`.
fn make_parents(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| {
            Error::io(
                ErrorKind::Io,
                format!("cannot open lock file {}", path.display()),
                source,
            )
        })?;
    }
    Ok(())
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
