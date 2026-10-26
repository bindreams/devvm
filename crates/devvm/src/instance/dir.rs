//! One placed instance: reading and updating its record under the op lock, and removing it.

use std::path::PathBuf;
use std::sync::Arc;

use crate::backend::MachineRecord;
use crate::exit::Progress;
use crate::instance::locks::{Locks, Probe};
use crate::instance::name::InstanceName;
use crate::instance::record::{InstanceId, InstanceRecord, Marker, MarkerRead, RecordRead, write_marker};
use crate::store::lock::FileLock;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "dir_tests.rs"]
mod dir_tests;

/// A handle on the instance `instances/<name>`, remembering which instance (by id) that name was when it was opened.
///
/// A name can be removed and reused while a handle waits for a lock, so every record read and every lock taken
/// checks that the instance behind the name still has the handle's id; otherwise the instance is gone.
pub(crate) struct InstanceDir {
    id: InstanceId,
    name: InstanceName,
    path: PathBuf,
    locks: Arc<Locks>,
    progress: Arc<dyn Progress>,
}

/// Proof of the exclusive op lock of one instance, needed to change its record or remove it.
pub(crate) struct OpExclusive {
    _lock: FileLock,
    id: InstanceId,
}

/// Proof of the shared op lock of one instance (`run`, `ssh`, `cp`). It cannot change or remove anything.
pub(crate) struct OpShared {
    _lock: FileLock,
}

impl InstanceDir {
    pub(super) fn new(
        id: InstanceId,
        name: InstanceName,
        path: PathBuf,
        locks: Arc<Locks>,
        progress: Arc<dyn Progress>,
    ) -> InstanceDir {
        InstanceDir {
            id,
            name,
            path,
            locks,
            progress,
        }
    }

    pub(crate) fn id(&self) -> &InstanceId {
        &self.id
    }

    pub(crate) fn name(&self) -> &InstanceName {
        &self.name
    }

    /// The instance directory (`disk.img`, `serial.log`, `record.json`, `owner.json`).
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn gone(&self) -> Error {
        Error::new(ErrorKind::NotFound, format!("instance {} is gone", self.name))
    }

    /// The record on disk. `NotFound` unless it is this handle's instance.
    pub(crate) fn record(&self) -> Result<InstanceRecord> {
        match self.locks.read_record(&self.path)? {
            RecordRead::Valid(record) if record.id == self.id && record.name == self.name => Ok(*record),
            RecordRead::Valid(record) if record.id == self.id => Err(Error::new(
                ErrorKind::Store,
                format!(
                    "the record in the directory of instance {} is for an instance called {}: {}",
                    self.name,
                    record.name,
                    self.path.join("record.json").display()
                ),
            )),
            RecordRead::Valid(_) | RecordRead::Absent => Err(self.gone()),
            RecordRead::Corrupt(why) => Err(Error::new(
                ErrorKind::Store,
                format!(
                    "the record of instance {} is unreadable ({why}): {}",
                    self.name,
                    self.path.join("record.json").display()
                ),
            )),
        }
    }

    /// The owner marker on disk, whichever instance it names.
    pub(crate) fn marker(&self) -> Result<MarkerRead> {
        self.locks.read_marker(&self.path)
    }

    /// Writes the owner marker `marker` into this instance's directory, after `before_write` ran. The directory cannot
    /// be renamed meanwhile, and it must still be this handle's instance: `NotFound` otherwise, with nothing written.
    pub(super) fn write_marker(&self, marker: &Marker, before_write: &mut dyn FnMut()) -> Result<()> {
        self.locks
            .reading(&self.path, || match InstanceRecord::read(&self.path)? {
                RecordRead::Valid(record) if record.id == self.id => {
                    before_write();
                    write_marker(&self.path, marker)
                }
                _ => Err(self.gone()),
            })?
            .unwrap_or_else(|| Err(self.gone()))
    }

    fn what(&self) -> String {
        format!("another devvm command on instance {}", self.name)
    }

    /// Waits for the exclusive op lock (`start`, `stop`, `rm`). `NotFound` if the instance went away meanwhile.
    pub(crate) fn op_exclusive(&self) -> Result<OpExclusive> {
        let lock = self.locks.op_exclusive(&self.id, &self.what(), &*self.progress)?;
        self.record()?;
        Ok(OpExclusive {
            _lock: lock,
            id: self.id,
        })
    }

    /// Waits for the shared op lock (`run`, `ssh`, `cp`). `NotFound` if the instance went away meanwhile.
    pub(crate) fn op_shared(&self) -> Result<OpShared> {
        let lock = self.locks.op_shared(&self.id, &self.what(), &*self.progress)?;
        self.record()?;
        Ok(OpShared { _lock: lock })
    }

    /// The exclusive op lock if nobody holds it, else `None`. `NotFound` if the instance is gone.
    pub(crate) fn try_op_exclusive(&self) -> Result<Option<OpExclusive>> {
        match self.locks.try_op_exclusive(&self.id)? {
            Probe::Free(lock) => {
                self.record()?;
                Ok(Some(OpExclusive {
                    _lock: lock,
                    id: self.id,
                }))
            }
            Probe::Held => Ok(None),
            Probe::Gone => Err(self.gone()),
        }
    }

    fn check(&self, op: &OpExclusive) -> Result<()> {
        if op.id == self.id {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::Internal,
                format!("the op lock of instance {} was used on instance {}", op.id, self.id),
            ))
        }
    }

    /// Rewrites the record: reads it, applies `f`, writes it back atomically.
    pub(crate) fn update(&self, op: &OpExclusive, f: &mut dyn FnMut(&mut InstanceRecord)) -> Result<InstanceRecord> {
        self.check(op)?;
        let mut record = self.record()?;
        f(&mut record);
        record.write(&self.path)?;
        Ok(record)
    }

    /// The machine named by `machine` is dead: deletes what it left outside the instance directory (a file that
    /// cannot be deleted goes to `warn`) and clears the record that names it. Call it only once the machine is proven
    /// dead, by a kill or by the backend not finding it.
    pub(crate) fn retire_machine(
        &self,
        op: &OpExclusive,
        machine: &MachineRecord,
        warn: &mut dyn FnMut(String),
    ) -> Result<()> {
        machine.remove_leftovers(warn);
        self.update(op, &mut |record| record.machine = None).map(drop)
    }

    /// Removes the instance: the name vanishes at once (a rename), then the files and the lock files go.
    ///
    /// A rename that fails deletes nothing and is an error naming the instance; a retry may succeed. devvm's own
    /// readers cannot cause it (the rename excludes them); Windows refuses it while another program has a file inside
    /// open. Failing to delete the renamed directory is only a warning: the reaper finishes it.
    pub(crate) fn destroy(&self, op: OpExclusive) -> Result<()> {
        self.destroy_with(op, &mut || {})
    }

    /// `destroy` with `before_wait` run just before it waits for the readers of instance files to finish.
    pub(crate) fn destroy_with(&self, op: OpExclusive, before_wait: &mut dyn FnMut()) -> Result<()> {
        self.destroy_with_waits(op, before_wait, &mut || {})
    }

    /// `destroy_with`, and `before_delete_wait` run just before it waits for another deletion to finish.
    pub(crate) fn destroy_with_waits(
        &self,
        op: OpExclusive,
        before_wait: &mut dyn FnMut(),
        before_delete_wait: &mut dyn FnMut(),
    ) -> Result<()> {
        self.check(&op)?;
        self.record()?;
        let dead = self
            .path
            .parent()
            .expect("an instance path has a parent")
            .join(format!(".dead-{}", self.id));
        self.locks
            .excluding_readers(before_wait, || std::fs::rename(&self.path, &dead))?
            .map_err(|e| {
                Error::io(
                    ErrorKind::Io,
                    format!(
                        "cannot remove instance {}: cannot rename {} (does a program have a file inside open? \
                         close it and retry)",
                        self.name,
                        self.path.display()
                    ),
                    e,
                )
            })?;
        // Deleting is done one tree at a time. This waits for a deletion in progress (it only deletes, so it ends) and
        // then deletes this tree: `rm` promises it is gone.
        match self.locks.deleting_waiting(before_delete_wait, || remove_tree(&dead)) {
            Ok(Err(e)) => self.progress.message(&format!(
                "instance {} is removed, but {} could not be deleted ({e}); the next devvm command retries",
                self.name,
                dead.display()
            )),
            Ok(Ok(())) => {}
            Err(e) => self.progress.message(&format!(
                "instance {} is removed, but {} could not be deleted ({}); the next devvm command retries",
                self.name,
                dead.display(),
                e.message()
            )),
        }
        // The instance is gone either way; leftover lock files go in the next lock sweep.
        if let Err(e) = self.locks.remove(&self.id) {
            self.progress.message(&format!(
                "instance {} is removed, but its lock files could not be deleted ({}); the next devvm command retries",
                self.name,
                e.message()
            ));
        }
        Ok(())
    }
}

/// Deletes `path` and everything under it. A tree that is gone afterwards is success, even when the error came from
/// a process outside devvm deleting it at the same time.
pub(super) fn remove_tree(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(_) if !path.exists() => Ok(()),
        other => other,
    }
}
