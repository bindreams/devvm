//! The per-instance OS locks. Both live in one directory, outside the instance directories, keyed by instance id:
//!
//! - `instance-<id>.owner`: held (exclusive) by whoever keeps the instance alive. The reaper may act on an instance
//!   only after taking it.
//! - `instance-<id>.op`: taken exclusive by `start`, `stop` and `rm`, shared by `run`, `ssh` and `cp`.
//!
//! - `instances.dir`: a leaf lock around the files inside instance directories and around the lock files. Whoever reads
//!   `record.json` or `owner.json`, opens a lock file or checks that a lock is still on the file at its name holds it
//!   shared; whoever renames an instance directory (`rm`, placing a new instance, quarantine) or unlinks lock files
//!   holds it exclusive. Windows refuses to rename a directory while any file inside is open, so without it a reader
//!   makes `rm` fail; and it refuses to open a file that is being unlinked with "access denied" instead of "not
//!   found". An unlink must therefore free the name at once, with handles open: POSIX delete semantics, which
//!   `std::fs::remove_file` uses on NTFS and ReFS. On exFAT, FAT and SMB a lock file with open handles stays
//!   delete-pending and opens still fail, so the state directory must be on NTFS or ReFS, and every open of
//!   `instances.dir` checks its volume. Its holders only read files, open files, look at a name, rename or unlink,
//!   never wait for another lock, so waiting for it cannot deadlock, not even in the layer store's collection, which
//!   must not wait for any lock that a holder of the shared `gc.lock` could hold. A creator makes it before it places
//!   an instance and nothing deletes it. A reader looks at the directory it will read under it, except for the first
//!   look when `instances.dir` is missing (see [`Locks::reading`]), and puts the lock file back if that directory
//!   exists without it.
//! - `dead.lock`: held by whoever deletes the contents of a `.dead-<id>` directory, so that two processes never delete
//!   one tree at once. `rm` waits for it (it promises its tree is gone); the reaper only tries it, since it runs again.
//!   Its holders only delete and wait for nothing.
//!
//! Nothing but a holder of the owner lock (or, in `destroy`, of the op lock) unlinks a lock file. A process that
//! opened a lock file before it was unlinked can still lock it, and then holds a lock on a file nobody else will ever
//! see, so every take of an instance lock compares the locked file with the file at the name, under `instances.dir`
//! shared: a file put at the name since is another file, and a lock on the old one proves nothing.
//!
//! No lock file is opened through a link: a link at a lock name is an error at the open, whatever its target holds.

use std::path::{Path, PathBuf};

use crate::exit::{NullProgress, Progress};
use crate::host::{Comparison, Host, symlink_metadata_if_present};
use crate::instance::record::{InstanceId, InstanceRecord, MarkerRead, RecordRead, read_marker};
use crate::store::lock::{FileLock, Mode};
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "locks_tests.rs"]
mod locks_tests;

pub(crate) struct Locks {
    dir: PathBuf,
    host: &'static dyn Host,
}

/// The result of trying a lock without waiting.
pub(crate) enum Probe {
    /// Nobody held it; the lock is now ours.
    Free(FileLock),
    Held,
    /// The lock file does not exist: the instance is gone, or was never there.
    Gone,
}

/// A lock file opened while `instances.dir` is held shared.
struct LockInDir {
    /// `instances.dir`, held shared until this is dropped.
    dir: FileLock,
    file: FileLock,
}

fn gone(id: &InstanceId) -> Error {
    Error::new(ErrorKind::NotFound, format!("instance {id} is gone"))
}

impl Locks {
    pub(crate) fn new(dir: PathBuf, host: &'static dyn Host) -> Locks {
        Locks { dir, host }
    }

    pub(crate) fn owner_path(&self, id: &InstanceId) -> PathBuf {
        self.dir.join(format!("instance-{id}.owner"))
    }

    pub(crate) fn op_path(&self, id: &InstanceId) -> PathBuf {
        self.dir.join(format!("instance-{id}.op"))
    }

    /// Runs `f` while no instance directory is renamed, if the instance directory `seen` exists; else `None`. `f` may
    /// read and write files inside the directory, but must not take or wait for any other lock.
    ///
    /// `seen` is looked at under `instances.dir`, so no devvm rename of it can run during the look. A reader of a state
    /// that never held an instance creates nothing: with no `instances.dir`, a `seen` that is not there ends the read,
    /// a look that fails is the error, and a `seen` that is there is looked at again under the lock once it is made. A
    /// creator makes `instances.dir` before it places any instance and nothing deletes it: a directory that exists
    /// while the lock file does not was damaged from outside, and the reader puts the file back rather than report
    /// that there is nothing to read.
    pub(crate) fn reading<T>(&self, seen: &Path, f: impl FnOnce() -> T) -> Result<Option<T>> {
        self.reading_with(seen, &mut || {}, f)
    }

    /// `reading` with `after_seen` run when `seen` was found while `instances.dir` did not exist, before it is made.
    pub(crate) fn reading_with<T>(
        &self,
        seen: &Path,
        after_seen: &mut dyn FnMut(),
        f: impl FnOnce() -> T,
    ) -> Result<Option<T>> {
        // A link at `seen` is there; what is absent is the same as in `FileId::compare`.
        let look = || {
            symlink_metadata_if_present(seen)
                .map(|meta| meta.is_some())
                .map_err(|e| Error::io(ErrorKind::Io, format!("cannot look at {}", seen.display()), e))
        };
        let dir_lock = match self.open_dir(false)? {
            Some(dir_lock) => dir_lock,
            None => {
                // A first create may be placing and renaming `seen` right now, so only "not there" is an answer; a look
                // that fails is the error, and creates nothing.
                if !look()? {
                    return Ok(None);
                }
                after_seen();
                self.make_dir()?
            }
        };
        let _shared = take(dir_lock, Mode::Shared, &mut || {})?;
        Ok(look()?.then(f))
    }

    /// Runs `f`, which renames an instance directory or unlinks lock files, while no one reads inside any instance
    /// directory or opens a lock file. `before_wait` runs just before waiting for them. `f` must not take or wait for
    /// any other lock.
    pub(crate) fn excluding_readers<T>(&self, before_wait: &mut dyn FnMut(), f: impl FnOnce() -> T) -> Result<T> {
        let dir = self.make_dir()?;
        let _lock = take(dir, Mode::Exclusive, before_wait)?;
        Ok(f())
    }

    /// Runs `f`, which deletes the contents of a `.dead-<id>` directory, unless another process is deleting one: then
    /// `None`, and the next devvm command's reaper covers this tree. Never waits. `f` must not take or wait for any
    /// other lock. The reaper uses this, since it has other work and runs again.
    ///
    /// Two processes deleting one tree make each other fail on Windows, and a lock on the tree's instance cannot
    /// serialize them: its lock files are unlinked at the end of `rm`, and a file that is being unlinked cannot be
    /// opened.
    pub(crate) fn deleting<T>(&self, f: impl FnOnce() -> T) -> Result<Option<T>> {
        let mut lock = FileLock::open(&self.dir.join("dead.lock"))?;
        if !lock.try_lock(Mode::Exclusive)? {
            return Ok(None);
        }
        Ok(Some(f()))
    }

    /// Like [`Locks::deleting`], but waits for the other deletion to finish, with `before_wait` run just before. `rm`
    /// uses this: it promises its tree is gone. Holders only delete a tree and wait for nothing, so this cannot
    /// deadlock, whatever lock the caller holds.
    pub(crate) fn deleting_waiting<T>(&self, before_wait: &mut dyn FnMut(), f: impl FnOnce() -> T) -> Result<T> {
        let _lock = take(
            FileLock::open(&self.dir.join("dead.lock"))?,
            Mode::Exclusive,
            before_wait,
        )?;
        Ok(f())
    }

    /// `record.json` of the instance directory `dir`, read while no directory is renamed.
    pub(crate) fn read_record(&self, dir: &Path) -> Result<RecordRead> {
        self.reading(dir, || InstanceRecord::read(dir))?
            .unwrap_or(Ok(RecordRead::Absent))
    }

    /// `owner.json` of the instance directory `dir`, read while no directory is renamed.
    pub(crate) fn read_marker(&self, dir: &Path) -> Result<MarkerRead> {
        self.reading(dir, || read_marker(dir))?
            .unwrap_or(Ok(MarkerRead::Absent))
    }

    /// Creates the owner and op files for a new instance and returns the owner lock, held.
    pub(crate) fn acquire_owner_new(&self, id: &InstanceId) -> Result<FileLock> {
        self.acquire_owner_new_with(id, &mut || {})
    }

    /// `acquire_owner_new` with `between` run after the lock is taken and before the file is checked, so a test can
    /// play the sweeper that unlinks it.
    ///
    /// Loops until the file at the name is the one locked: each time it is not, a sweeper made progress and the next
    /// iteration creates a fresh file. The op file is created only once the owner lock is held on the file at the
    /// name, inside the hold of `instances.dir` that the check was made under, so no sweeper can remove it
    /// afterwards.
    pub(crate) fn acquire_owner_new_with(&self, id: &InstanceId, between: &mut dyn FnMut()) -> Result<FileLock> {
        let path = self.owner_path(id);
        loop {
            // The hold ends with the open: waiting for the owner lock must not block a sweeper's unlink.
            let mut owner = self.open_in_dir(&path, true, &mut || {})?.ok_or_else(|| gone(id))?.file;
            owner.lock(Mode::Exclusive, "a lock-file sweep to finish", &NullProgress)?;
            between();
            let (comparison, _dir) = self.is_current(&owner)?;
            if comparison == Comparison::Same {
                drop(FileLock::open(&self.op_path(id))?);
                return Ok(owner);
            }
        }
    }

    /// Tries the owner lock like [`Locks::try_owner`], but puts back a missing lock file instead of finding it gone.
    /// For an instance whose directory exists and whose record names this id: its lock files can only have been
    /// deleted from outside.
    ///
    /// A holder of an old, unlinked owner file is not seen. Like `acquire_owner_new_with`, it retries until the locked
    /// file is still the one at the name, and it never waits for a held lock. The op file is created inside the hold
    /// of `instances.dir` that the check was made under.
    pub(crate) fn try_owner_recreating(&self, id: &InstanceId) -> Result<Probe> {
        let op_path = self.op_path(id);
        let make_op = &mut || FileLock::open(&op_path).map(drop);
        self.probe(&self.owner_path(id), true, &mut || {}, &mut || {}, make_op)
    }

    /// Tries the owner lock without creating anything.
    pub(crate) fn try_owner(&self, id: &InstanceId) -> Result<Probe> {
        self.try_owner_with(id, &mut || {}, &mut || {})
    }

    pub(crate) fn try_owner_with(
        &self,
        id: &InstanceId,
        before_wait: &mut dyn FnMut(),
        between: &mut dyn FnMut(),
    ) -> Result<Probe> {
        self.probe(&self.owner_path(id), false, before_wait, between, &mut || Ok(()))
    }

    /// Tries the exclusive op lock without creating anything.
    pub(crate) fn try_op_exclusive(&self, id: &InstanceId) -> Result<Probe> {
        self.probe(&self.op_path(id), false, &mut || {}, &mut || {}, &mut || Ok(()))
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
        loop {
            let mut lock = self
                .open_in_dir(&path, false, &mut || {})?
                .ok_or_else(|| gone(id))?
                .file;
            lock.lock(mode, what, progress)?;
            // The holder we waited for may have removed the instance, or put another file at the name.
            match self.is_current(&lock)?.0 {
                Comparison::Same => return Ok(lock),
                Comparison::Absent => return Err(gone(id)),
                Comparison::Different => {}
            }
        }
    }

    /// Unlinks both lock files while no one opens a lock file. An unlink that fails stops it, leaves the owner file for
    /// the next lock sweep, and is the error. For a holder of the owner lock (reaper, `Owned`) or of the op lock.
    pub(crate) fn remove(&self, id: &InstanceId) -> Result<()> {
        self.remove_with(id, &mut || {})
    }

    /// `remove` with `before_wait` run just before it waits for whoever is opening a lock file.
    pub(crate) fn remove_with(&self, id: &InstanceId, before_wait: &mut dyn FnMut()) -> Result<()> {
        self.remove_via(id, before_wait, &|path| std::fs::remove_file(path))
    }

    /// `remove_with` unlinking through `unlink`.
    ///
    /// The owner file goes only after the op file did: a failure leaves the owner file, which is what the lock sweep
    /// looks for (an op file alone would be listed but never reported free).
    fn remove_via(
        &self,
        id: &InstanceId,
        before_wait: &mut dyn FnMut(),
        unlink: &dyn Fn(&Path) -> std::io::Result<()>,
    ) -> Result<()> {
        self.excluding_readers(before_wait, || {
            for path in [self.op_path(id), self.owner_path(id)] {
                match unlink(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(Error::io(
                            ErrorKind::Io,
                            format!("cannot delete the lock file {}", path.display()),
                            e,
                        ));
                    }
                }
            }
            Ok(())
        })?
    }

    /// How the file at the path of `lock`, taken, compares with the file `lock` is on, and the shared hold of
    /// `instances.dir` it was looked under. Looks while no lock file is unlinked, so the name is there or not there,
    /// with no unlink half done. `lock` stays open across the look: the file system gives its identity to another
    /// file only after the last handle closes.
    ///
    /// `instances.dir` is made if it is missing: devvm never deletes it, so a missing one was damaged from outside,
    /// and a lock file is not gone for that, as in [`Locks::reading`].
    fn is_current(&self, lock: &FileLock) -> Result<(Comparison, FileLock)> {
        let dir = self.make_dir()?;
        let dir = take(dir, Mode::Shared, &mut || {})?;
        Ok((lock.is_at_path()?, dir))
    }

    /// Opens `instances.dir`, making it if `create`, else `None` if it is absent; not locked. Its volume is the volume
    /// of every lock file in it, so the places that open it check it.
    fn open_dir(&self, create: bool) -> Result<Option<FileLock>> {
        FileLock::open_with(&self.dir.join("instances.dir"), create)?
            .map(|dir| self.checked(dir))
            .transpose()
    }

    /// [`Locks::open_dir`] with `create`, whose result is never `None`.
    fn make_dir(&self) -> Result<FileLock> {
        self.open_dir(true)?
            .ok_or_else(|| Error::new(ErrorKind::Internal, "opening instances.dir with create gave no file"))
    }

    /// `dir`, the lock file of `instances.dir`, if its volume allows the lock design.
    fn checked(&self, dir: FileLock) -> Result<FileLock> {
        if !dir.volume_has_posix_unlink_rename(self.host)? {
            return Err(Error::new(
                ErrorKind::NotSupported,
                format!(
                    "the file system of {} has no POSIX unlink and rename: the devvm state directory must be on NTFS or \
                     ReFS",
                    self.dir.display()
                ),
            ));
        }
        Ok(dir)
    }

    /// Opens the lock file at `path` under a shared hold of `instances.dir` that lasts as long as the result does, so a
    /// caller that is about to wait for the opened lock drops it first; `before_wait` runs just before waiting for the
    /// hold. With `create`, makes the file and `instances.dir` if missing; else `None` if either is absent.
    fn open_in_dir(&self, path: &Path, create: bool, before_wait: &mut dyn FnMut()) -> Result<Option<LockInDir>> {
        let Some(dir) = self.open_dir(create)? else {
            return Ok(None);
        };
        let dir = take(dir, Mode::Shared, before_wait)?;
        Ok(FileLock::open_with(path, create)?.map(|file| LockInDir { dir, file }))
    }

    /// Tries the lock file at `path` exclusive without waiting, and compares the file at its name with the one locked,
    /// all under the hold of `instances.dir` it was opened with: no unlink or rename of a lock file runs between the
    /// steps. `between` runs before the try; `on_free` runs when the lock is ours, still under the hold.
    ///
    /// A file put at the name since it was opened is another file: the try starts over from the open. No file at the
    /// name is `Gone`, or with `create` a file to make again.
    fn probe(
        &self,
        path: &Path,
        create: bool,
        before_wait: &mut dyn FnMut(),
        between: &mut dyn FnMut(),
        on_free: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Probe> {
        loop {
            let Some(LockInDir {
                dir: _dir,
                file: mut lock,
            }) = self.open_in_dir(path, create, before_wait)?
            else {
                return Ok(Probe::Gone);
            };
            between();
            if !lock.try_lock(Mode::Exclusive)? {
                return Ok(Probe::Held);
            }
            match lock.is_at_path()? {
                Comparison::Same => {
                    on_free()?;
                    return Ok(Probe::Free(lock));
                }
                Comparison::Absent if !create => return Ok(Probe::Gone),
                Comparison::Absent | Comparison::Different => {}
            }
        }
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
