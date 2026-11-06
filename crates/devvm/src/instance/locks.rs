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
//!   an instance and nothing deletes it. A reader looks at the directory or lock file it will read under it, except
//!   for the first look when `instances.dir` is missing (see [`Locks::reading`]), and puts `instances.dir` back if
//!   what it reads exists without it.
//! - `dead.lock`: held by whoever deletes a tree devvm owns (the contents of a `.dead-<id>` or `.new-<id>` directory),
//!   so that two processes never delete one tree at once. `rm` waits for it (it promises its tree is gone); the reaper
//!   only tries it, since it runs again. Its holders only delete and wait for nothing.
//!
//! Nothing but a holder of the owner lock (or, in `destroy`, of the op lock) unlinks a lock file. A process that
//! opened a lock file before it was unlinked can still lock it, and then holds a lock on a file nobody else will ever
//! see, so every take of an instance lock compares the locked file with the file at the name, under `instances.dir`
//! shared: a file put at the name since is another file, and a lock on the old one proves nothing. `instances.dir`
//! itself is compared the same way at every take.
//!
//! No lock file is opened through a link: a link at a lock name is an error at the open.

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

/// A lock file with the shared hold of `instances.dir` it was opened under.
struct LockInDir {
    dir: FileLock,
    file: FileLock,
}

/// Where the lock file of a [`Take`] is.
#[derive(Clone, Copy)]
enum Scope {
    /// `instances.dir` itself: nothing holds it around the look, and its volume is checked.
    Dir,
    /// A file in the lock directory: `instances.dir` is held shared around the look.
    InDir,
}

/// One take of a lock file: what it locks and how.
struct Take<'a> {
    scope: Scope,
    mode: Mode,
    /// Makes the file and `instances.dir` if missing. Else no file at the name is [`Current::Gone`]. A waiting
    /// [`Scope::InDir`] take makes `instances.dir` for the look whatever `create` is.
    create: bool,
    /// `None` tries the lock: [`Current::Held`] if another holds it, and the shared hold of `instances.dir` lasts from
    /// the open to the end of the look. Else waits for the lock without that hold, so as not to block an unlinker,
    /// saying `waiting for <what>` through `progress`.
    wait: Option<(&'a str, &'a dyn Progress)>,
}

/// How a take ended.
enum Current {
    /// No file at the name: before the open, or, without `create`, after the lock.
    Gone,
    Held,
    /// The lock is ours, on the file at the name, and `dir` is the shared hold of `instances.dir` the look was made
    /// under (`None` for [`Scope::Dir`]).
    Locked {
        lock: FileLock,
        dir: Option<FileLock>,
    },
}

fn gone(id: &InstanceId) -> Error {
    Error::new(ErrorKind::NotFound, format!("instance {id} is gone"))
}

impl Current {
    /// For a take that waits, which is never [`Current::Held`]: the lock and its hold, or `None` if gone.
    fn waited(self) -> Option<(FileLock, Option<FileLock>)> {
        match self {
            Current::Gone => None,
            Current::Held => unreachable!("a take that waits is never held"),
            Current::Locked { lock, dir } => Some((lock, dir)),
        }
    }
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
    /// `seen` is looked at under `instances.dir`, so no devvm rename of it runs during the look. With no
    /// `instances.dir`, nothing is created unless `seen` exists; a look that fails is the error.
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
        let Some(_dir) = self.shared_dir_for(seen, after_seen, &mut || {})? else {
            return Ok(None);
        };
        Ok(is_there(seen)?.then(f))
    }

    /// Runs `f`, which renames an instance directory or unlinks lock files, while no one reads inside any instance
    /// directory or opens a lock file. `before_wait` runs just before waiting for them. `f` must not take or wait for
    /// any other lock.
    pub(crate) fn excluding_readers<T>(&self, before_wait: &mut dyn FnMut(), f: impl FnOnce() -> T) -> Result<T> {
        let _dir = self.make_dir(Mode::Exclusive, before_wait)?;
        Ok(f())
    }

    /// Runs `f`, which deletes a tree devvm owns (a `.dead-<id>` or `.new-<id>` directory), unless another process is
    /// deleting one: then `None`, and the next devvm command's reaper covers this tree. Never waits. `f` must not take
    /// or wait for any other lock. The reaper uses this, since it has other work and runs again.
    ///
    /// Two processes deleting one tree make each other fail on Windows, and a lock on the tree's instance cannot
    /// serialize them: its lock files are unlinked at the end of `rm`, and a file that is being unlinked cannot be
    /// opened.
    pub(crate) fn deleting<T>(&self, f: impl FnOnce() -> T) -> Result<Option<T>> {
        let mut lock = self.create(&self.dir.join("dead.lock"))?;
        if !lock.try_lock(Mode::Exclusive)? {
            return Ok(None);
        }
        Ok(Some(f()))
    }

    /// Like [`Locks::deleting`], but waits for the other deletion to finish, with `before_wait` run just before. `rm`
    /// uses this: it promises its tree is gone. Holders only delete a tree and wait for nothing, so this cannot
    /// deadlock, whatever lock the caller holds.
    pub(crate) fn deleting_waiting<T>(&self, before_wait: &mut dyn FnMut(), f: impl FnOnce() -> T) -> Result<T> {
        let mut lock = self.create(&self.dir.join("dead.lock"))?;
        lock.lock(Mode::Exclusive, DIR_WAIT, &mut |_| before_wait())?;
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
        self.acquire_owner_new_with(id, &mut || {}, &mut || {})
    }

    /// `acquire_owner_new` with `between` run after the file is opened, so a test can play the sweeper that unlinks it,
    /// and `before_look` run under the shared hold of `instances.dir`, just before the file at the name is checked.
    ///
    /// Loops until the file at the name is the one locked: each time it is not, a sweeper made progress and the next
    /// iteration creates a fresh file. The op file is created only once the owner lock is held on the file at the
    /// name, inside the hold of `instances.dir` that the check was made under, so no sweeper can remove it
    /// afterwards.
    pub(crate) fn acquire_owner_new_with(
        &self,
        id: &InstanceId,
        between: &mut dyn FnMut(),
        before_look: &mut dyn FnMut(),
    ) -> Result<FileLock> {
        let take = Take {
            scope: Scope::InDir,
            mode: Mode::Exclusive,
            create: true,
            wait: Some(("a lock-file sweep to finish", &NullProgress)),
        };
        let (owner, _dir) = self
            .take_current(&self.owner_path(id), &take, &mut || {}, between, before_look)?
            .waited()
            .expect("a take that creates finds its file");
        drop(self.create(&self.op_path(id))?);
        Ok(owner)
    }

    /// Tries the owner lock like [`Locks::try_owner`], but puts back a missing lock file instead of finding it gone.
    /// For an instance whose directory exists and whose record names this id: its lock files can only have been
    /// deleted from outside.
    ///
    /// A holder of an old, unlinked owner file is not seen. Like `acquire_owner_new_with`, it retries until the locked
    /// file is still the one at the name, and it never waits for a held lock.
    pub(crate) fn try_owner_recreating(&self, id: &InstanceId) -> Result<Probe> {
        self.try_owner_recreating_with(id, &mut || {}, &mut || {})
    }

    /// `try_owner_recreating` with `between` run after the file is opened and before it is tried, so a test can play
    /// the sweeper that unlinks it, and `before_op` run just before the op file is made.
    pub(crate) fn try_owner_recreating_with(
        &self,
        id: &InstanceId,
        between: &mut dyn FnMut(),
        before_op: &mut dyn FnMut(),
    ) -> Result<Probe> {
        let op_path = self.op_path(id);
        let make_op = &mut || {
            before_op();
            self.create(&op_path).map(drop)
        };
        self.probe(&self.owner_path(id), true, &mut || {}, between, make_op)
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
        self.op_with(id, Mode::Exclusive, what, progress, &mut || {}, &mut || {})
    }

    /// Takes the op lock shared, waiting for it. `NotFound` if the instance is gone before or after the wait.
    pub(crate) fn op_shared(&self, id: &InstanceId, what: &str, progress: &dyn Progress) -> Result<FileLock> {
        self.op_with(id, Mode::Shared, what, progress, &mut || {}, &mut || {})
    }

    /// The op lock in `mode`, with `before_wait` run just before the take first waits, and `before_look` run under the
    /// shared hold of `instances.dir`, just before the file at the name is checked. The holder waited for may have
    /// removed the instance or put another file at the name.
    fn op_with(
        &self,
        id: &InstanceId,
        mode: Mode,
        what: &str,
        progress: &dyn Progress,
        before_wait: &mut dyn FnMut(),
        before_look: &mut dyn FnMut(),
    ) -> Result<FileLock> {
        let take = Take {
            scope: Scope::InDir,
            mode,
            create: false,
            wait: Some((what, progress)),
        };
        let taken = self.take_current(&self.op_path(id), &take, before_wait, &mut || {}, before_look)?;
        taken.waited().map(|(lock, _dir)| lock).ok_or_else(|| gone(id))
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

    /// `instances.dir`, taken in `mode`, whose file is the one at its name. Made if `create`, else `None` if it is
    /// absent. `before_wait` runs just before waiting for the lock. Its volume is the volume of every lock file in it,
    /// so every take checks it.
    fn lock_dir(&self, create: bool, mode: Mode, before_wait: &mut dyn FnMut()) -> Result<Option<FileLock>> {
        let take = Take {
            scope: Scope::Dir,
            mode,
            create,
            wait: Some((DIR_WAIT, &NullProgress)),
        };
        let taken = self.take_current(
            &self.dir.join("instances.dir"),
            &take,
            before_wait,
            &mut || {},
            &mut || {},
        )?;
        Ok(taken.waited().map(|(lock, _)| lock))
    }

    /// [`Locks::lock_dir`] with `create`, whose result is never `None`.
    fn make_dir(&self, mode: Mode, before_wait: &mut dyn FnMut()) -> Result<FileLock> {
        Ok(self
            .lock_dir(true, mode, before_wait)?
            .expect("a take that creates finds its file"))
    }

    /// The lock file at `path`, made with its directories if missing, opened through this host.
    fn create(&self, path: &Path) -> Result<FileLock> {
        FileLock::create_with_host(self.host, path)
    }

    /// [`Locks::create`] if `create`, else the existing file or `None`.
    fn open_file(&self, path: &Path, create: bool) -> Result<Option<FileLock>> {
        if create {
            self.create(path).map(Some)
        } else {
            FileLock::open_existing_with_host(self.host, path)
        }
    }

    /// `instances.dir` held shared for a look at or an open of `seen`. With no `instances.dir`, `None` if `seen` is not
    /// there, and nothing is made; else it is made, after `after_seen`. A first create may be placing and renaming
    /// `seen` right now, so only "not there" is an answer.
    fn shared_dir_for(
        &self,
        seen: &Path,
        after_seen: &mut dyn FnMut(),
        before_wait: &mut dyn FnMut(),
    ) -> Result<Option<FileLock>> {
        if let Some(dir) = self.lock_dir(false, Mode::Shared, before_wait)? {
            return Ok(Some(dir));
        }
        if !is_there(seen)? {
            return Ok(None);
        }
        after_seen();
        self.make_dir(Mode::Shared, before_wait).map(Some)
    }

    /// `dir`, the lock file of `instances.dir`, if its volume allows the lock design.
    fn checked(&self, dir: FileLock) -> Result<FileLock> {
        if !dir.volume_has_posix_unlink_rename()? {
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
    /// hold. With `create`, makes the file and `instances.dir` if missing; else `None` if the file is absent.
    fn open_in_dir(&self, path: &Path, create: bool, before_wait: &mut dyn FnMut()) -> Result<Option<LockInDir>> {
        let dir = if create {
            Some(self.make_dir(Mode::Shared, before_wait)?)
        } else {
            self.shared_dir_for(path, &mut || {}, before_wait)?
        };
        let Some(dir) = dir else {
            return Ok(None);
        };
        Ok(self.open_file(path, create)?.map(|file| LockInDir { dir, file }))
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
        let take = Take {
            scope: Scope::InDir,
            mode: Mode::Exclusive,
            create,
            wait: None,
        };
        match self.take_current(path, &take, before_wait, between, &mut || {})? {
            Current::Gone => Ok(Probe::Gone),
            Current::Held => Ok(Probe::Held),
            Current::Locked { lock, dir: _dir } => {
                on_free()?;
                Ok(Probe::Free(lock))
            }
        }
    }

    /// Opens the lock file at `path` and locks it as `take` says, until the file at the name is the one locked: each
    /// time it is not, the take starts over from the open. The look is made under the shared hold of `instances.dir`
    /// ([`Scope::InDir`]), so no unlink or rename of a lock file runs during it. No file at the name is
    /// [`Current::Gone`] without `create`, else a file to make again.
    ///
    /// `before_wait` runs at most once, just before the take first waits. `between` runs after each open, with the hold
    /// of `instances.dir` only if the lock is tried, `before_look` just before each look, under the hold.
    fn take_current(
        &self,
        path: &Path,
        take: &Take,
        before_wait: &mut dyn FnMut(),
        between: &mut dyn FnMut(),
        before_look: &mut dyn FnMut(),
    ) -> Result<Current> {
        // Whether the take has waited, for any lock, and whether it said what it waits for: a wait for `instances.dir`
        // is the first, not the second.
        let mut waited = false;
        let mut said = false;
        let mut before_wait = || {
            if !std::mem::replace(&mut waited, true) {
                before_wait();
            }
        };
        loop {
            let (hold, mut lock) = match take.scope {
                Scope::Dir => match self.open_file(path, take.create)? {
                    Some(file) => (None, self.checked(file)?),
                    None => return Ok(Current::Gone),
                },
                Scope::InDir => match self.open_in_dir(path, take.create, &mut before_wait)? {
                    Some(LockInDir { dir, file }) => (Some(dir), file),
                    None => return Ok(Current::Gone),
                },
            };
            // A wait ends the hold with the open: it must not block an unlinker.
            let hold = hold.filter(|_| take.wait.is_none());
            between();
            match take.wait {
                None => {
                    if !lock.try_lock(take.mode)? {
                        return Ok(Current::Held);
                    }
                }
                Some((what, progress)) => {
                    lock.lock(take.mode, what, &mut |message| {
                        before_wait();
                        if !std::mem::replace(&mut said, true) {
                            progress.message(message);
                        }
                    })?;
                }
            }
            let dir = match (take.scope, hold) {
                (Scope::Dir, _) => None,
                (Scope::InDir, Some(dir)) => Some(dir),
                (Scope::InDir, None) => Some(self.make_dir(Mode::Shared, &mut before_wait)?),
            };
            before_look();
            match lock.is_at_path()? {
                Comparison::Same => return Ok(Current::Locked { lock, dir }),
                Comparison::Absent if !take.create => return Ok(Current::Gone),
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

/// Whether anything is at `path`: a link at `path` counts as there, followed or not.
fn is_there(path: &Path) -> Result<bool> {
    symlink_metadata_if_present(path)
        .map(|meta| meta.is_some())
        .map_err(|e| Error::io(ErrorKind::Io, format!("cannot look at {}", path.display()), e))
}

/// What a wait for the locks of this directory is for.
const DIR_WAIT: &str = "devvm commands reading instances";
