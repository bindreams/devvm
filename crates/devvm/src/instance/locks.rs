//! The per-instance OS locks. Both live in one directory, outside the instance directories, keyed by instance id:
//!
//! - `instance-<id>.owner`: held (exclusive) by whoever keeps the instance alive. The reaper may act on an instance
//!   only after taking it.
//! - `instance-<id>.op`: taken exclusive by `start`, `stop` and `rm`, shared by `run`, `ssh` and `cp`.
//!
//! Nothing but a holder of the owner lock (or, in `destroy`, of the op lock) unlinks a lock file. A process that
//! opened a lock file before it was unlinked can still lock it, and then holds a lock on a file nobody else will ever
//! see, so every take verifies that the file is still there afterwards.

use std::path::PathBuf;

use crate::exit::{NullProgress, Progress};
use crate::instance::record::InstanceId;
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
