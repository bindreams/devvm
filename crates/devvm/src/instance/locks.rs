//! The per-instance OS locks. Both live in one directory, outside the instance directories, keyed by instance id:
//!
//! - `instance-<id>.owner`: held (exclusive) by whoever keeps the instance alive. The reaper may act on an instance
//!   only after taking it.
//! - `instance-<id>.op`: taken exclusive by `start`, `stop` and `rm`, shared by `run`, `ssh` and `cp`.
//!
//! - `instances.dir`: a leaf lock around the files inside instance directories. Whoever reads `record.json` or
//!   `owner.json` holds it shared; whoever renames an instance directory (`rm`, quarantine) holds it exclusive. Windows
//!   refuses to rename a directory while any file inside is open, so without it a reader makes `rm` fail. Its holders
//!   only read files or rename, never wait for another lock, so waiting for it cannot deadlock, not even in the layer
//!   store's collection, which must not wait for any lock that a holder of the shared `gc.lock` could hold.
//!
//! Nothing but a holder of the owner lock (or, in `destroy`, of the op lock) unlinks a lock file. A process that
//! opened a lock file before it was unlinked can still lock it, and then holds a lock on a file nobody else will ever
//! see, so every take verifies that the file is still there afterwards.

use std::path::{Path, PathBuf};

use crate::exit::{NullProgress, Progress};
use crate::instance::record::{InstanceId, InstanceRecord, MarkerRead, RecordRead, read_marker};
use crate::store::lock::{FileLock, Mode};
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "locks_tests.rs"]
mod locks_tests;

pub(crate) struct Locks {
    dir: PathBuf,
}

/// The result of trying a lock without waiting.
pub(crate) enum Probe {
    /// Nobody held it; the lock is now ours.
    Free(FileLock),
    Held,
    /// The lock file does not exist: the instance is gone, or was never there.
    Gone,
}

fn gone(id: &InstanceId) -> Error {
    Error::new(ErrorKind::NotFound, format!("instance {id} is gone"))
}

impl Locks {
    pub(crate) fn new(dir: PathBuf) -> Locks {
        Locks { dir }
    }

    pub(crate) fn owner_path(&self, id: &InstanceId) -> PathBuf {
        self.dir.join(format!("instance-{id}.owner"))
    }

    pub(crate) fn op_path(&self, id: &InstanceId) -> PathBuf {
        self.dir.join(format!("instance-{id}.op"))
    }

    /// Runs `f` while no instance directory is renamed. `f` must only read files: it must not take or wait for any
    /// other lock.
    pub(crate) fn reading<T>(&self, f: impl FnOnce() -> T) -> Result<T> {
        // No lock file means no instance was ever created, so no directory can be renamed: nothing to exclude.
        let _lock = FileLock::open_existing(&self.dir_lock_path())?
            .map(|lock| take(lock, Mode::Shared, &mut || {}))
            .transpose()?;
        Ok(f())
    }

    /// Runs `f`, which renames an instance directory, while no one reads inside any instance directory. `before_wait`
    /// runs just before waiting for the readers. `f` must not take or wait for any other lock.
    pub(crate) fn renaming<T>(&self, before_wait: &mut dyn FnMut(), f: impl FnOnce() -> T) -> Result<T> {
        let _lock = take(FileLock::open(&self.dir_lock_path())?, Mode::Exclusive, before_wait)?;
        Ok(f())
    }

    fn dir_lock_path(&self) -> PathBuf {
        self.dir.join("instances.dir")
    }

    /// `record.json` of the instance directory `dir`, read while no directory is renamed.
    pub(crate) fn read_record(&self, dir: &Path) -> Result<RecordRead> {
        self.reading(|| InstanceRecord::read(dir))?
    }

    /// `owner.json` of the instance directory `dir`, read while no directory is renamed.
    pub(crate) fn read_marker(&self, dir: &Path) -> Result<MarkerRead> {
        self.reading(|| read_marker(dir))?
    }

    /// Creates the owner and op files for a new instance and returns the owner lock, held.
    pub(crate) fn acquire_owner_new(&self, id: &InstanceId) -> Result<FileLock> {
        self.acquire_owner_new_with(id, &mut || {})
    }

    /// `acquire_owner_new` with `between` run after the lock is taken and before the file is checked, so a test can
    /// play the sweeper that unlinks it.
    ///
    /// Loops until the locked file is still linked: each time it is not, a sweeper made progress and the next
    /// iteration creates a fresh file. The op file is created only once the owner lock is held on a linked file, so no
    /// sweeper can remove it afterwards.
    pub(crate) fn acquire_owner_new_with(&self, id: &InstanceId, between: &mut dyn FnMut()) -> Result<FileLock> {
        let path = self.owner_path(id);
        // Every instance has this file from its creation on; `reading` relies on it.
        drop(FileLock::open(&self.dir_lock_path())?);
        loop {
            let mut owner = FileLock::open(&path)?;
            owner.lock(Mode::Exclusive, "a lock-file sweep to finish", &NullProgress)?;
            between();
            if path.exists() {
                drop(FileLock::open(&self.op_path(id))?);
                return Ok(owner);
            }
        }
    }

    /// Tries the owner lock like [`Locks::try_owner`], but first puts back a missing lock file. For an instance whose
    /// directory exists and whose record names this id: its lock files can only have been deleted from outside.
    ///
    /// A holder of an old, unlinked owner file is not seen. Like `acquire_owner_new_with`, it retries until the locked
    /// file is still linked, and it never waits for a held lock.
    pub(crate) fn try_owner_recreating(&self, id: &InstanceId) -> Result<Probe> {
        let path = self.owner_path(id);
        loop {
            let mut owner = FileLock::open(&path)?;
            if !owner.try_lock(Mode::Exclusive)? {
                return Ok(Probe::Held);
            }
            if path.exists() {
                drop(FileLock::open(&self.op_path(id))?);
                return Ok(Probe::Free(owner));
            }
        }
    }

    /// Tries the owner lock without creating anything.
    pub(crate) fn try_owner(&self, id: &InstanceId) -> Result<Probe> {
        self.try_owner_with(id, &mut || {})
    }

    pub(crate) fn try_owner_with(&self, id: &InstanceId, between: &mut dyn FnMut()) -> Result<Probe> {
        probe(&self.owner_path(id), between)
    }

    /// Tries the exclusive op lock without creating anything.
    pub(crate) fn try_op_exclusive(&self, id: &InstanceId) -> Result<Probe> {
        probe(&self.op_path(id), &mut || {})
    }

    /// Takes the op lock exclusive, waiting for it. `NotFound` if the instance is gone before or after the wait.
    pub(crate) fn op_exclusive(&self, id: &InstanceId, what: &str, progress: &dyn Progress) -> Result<FileLock> {
        self.op(id, Mode::Exclusive, what, progress)
    }

    /// Takes the op lock shared, waiting for it. `NotFound` if the instance is gone before or after the wait.
    pub(crate) fn op_shared(&self, id: &InstanceId, what: &str, progress: &dyn Progress) -> Result<FileLock> {
        self.op(id, Mode::Shared, what, progress)
    }

    fn op(&self, id: &InstanceId, mode: Mode, what: &str, progress: &dyn Progress) -> Result<FileLock> {
        let path = self.op_path(id);
        let mut lock = FileLock::open_existing(&path)?.ok_or_else(|| gone(id))?;
        lock.lock(mode, what, progress)?;
        // The holder we waited for may have removed the instance: we would then hold a lock on an unlinked file.
        if path.exists() { Ok(lock) } else { Err(gone(id)) }
    }

    /// Deletes both lock files, best effort. For a holder of the owner lock (reaper, `Owned`) or of the op lock.
    pub(crate) fn remove(&self, id: &InstanceId) {
        let _ = std::fs::remove_file(self.op_path(id));
        let _ = std::fs::remove_file(self.owner_path(id));
    }

    /// The ids that have a lock file, in no particular order.
    pub(crate) fn ids(&self) -> Result<Vec<InstanceId>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(Error::io(
                    ErrorKind::Io,
                    format!("cannot list {}", self.dir.display()),
                    e,
                ));
            }
        };
        let mut ids: Vec<InstanceId> = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|e| Error::io(ErrorKind::Io, format!("cannot list {}", self.dir.display()), e))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(stem) = name.strip_prefix("instance-") else {
                continue;
            };
            let Some(text) = stem.strip_suffix(".owner").or_else(|| stem.strip_suffix(".op")) else {
                continue;
            };
            if let Ok(id) = text.parse::<InstanceId>()
                && !ids.contains(&id)
            {
                ids.push(id);
            }
        }
        Ok(ids)
    }
}

/// Locks `lock`, saying so through `before_wait` first when it has to wait.
fn take(mut lock: FileLock, mode: Mode, before_wait: &mut dyn FnMut()) -> Result<FileLock> {
    if !lock.try_lock(mode)? {
        before_wait();
        lock.lock(mode, "devvm commands reading instances", &NullProgress)?;
    }
    Ok(lock)
}

fn probe(path: &std::path::Path, between: &mut dyn FnMut()) -> Result<Probe> {
    let Some(mut lock) = FileLock::open_existing(path)? else {
        return Ok(Probe::Gone);
    };
    between();
    if !lock.try_lock(Mode::Exclusive)? {
        return Ok(Probe::Held);
    }
    Ok(if path.exists() { Probe::Free(lock) } else { Probe::Gone })
}
