//! One placed instance: reading and updating its record under the op lock, and removing it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cosca::identity::{Liveness, ProcessId};

use crate::backend::{Backend, Machine, MachineRecord};
use crate::host::{Host, remove_tree};
use crate::instance::locks::{Locks, Probe};
use crate::instance::name::InstanceName;
use crate::instance::record::{
    Holder, InstanceId, InstanceRecord, Marker, MarkerRead, RECORD_FILE, RecordRead, read_holder, remove_holder,
    write_holder, write_marker,
};
use crate::progress::Progress;
use crate::store::lock::FileLock;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "dir_tests.rs"]
pub(crate) mod dir_tests;

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
///
/// Dropping it removes the holder record (see [`Holder`]) and then releases the lock: the record goes first, so the next
/// holder's record is never removed by this one.
pub(crate) struct OpExclusive {
    id: InstanceId,
    name: InstanceName,
    path: PathBuf,
    locks: Arc<Locks>,
    progress: Arc<dyn Progress>,
    _lock: FileLock,
}

impl Drop for OpExclusive {
    /// The holder record stays, with a warning, when it could not be shown to be this instance's or could not be removed.
    /// An instance that is gone or replaced has nothing of this holder's to remove.
    fn drop(&mut self) {
        let removed = in_instance(&self.locks, &self.name, &self.path, &self.id, || {
            remove_holder(&self.path)
        });
        if let Err(e) = removed {
            self.progress.warn(&format!(
                "instance {}: cannot remove the holder record ({})",
                self.name,
                e.message()
            ));
        }
    }
}

/// Runs `f` while no instance directory is renamed, if the directory at `path` is the instance `id`: `None` if it is
/// gone or another instance, `Store` if its record is unreadable.
fn in_instance<T>(
    locks: &Locks,
    name: &InstanceName,
    path: &Path,
    id: &InstanceId,
    f: impl FnOnce() -> Result<T>,
) -> Result<Option<T>> {
    locks
        .reading(path, || match InstanceRecord::read(path)? {
            RecordRead::Valid(record) if record.id == *id => f().map(Some),
            RecordRead::Corrupt(why) => Err(unreadable_record(name, path, &why)),
            RecordRead::Valid(_) | RecordRead::Absent => Ok(None),
        })?
        .unwrap_or(Ok(None))
}

fn unreadable_record(name: &InstanceName, path: &Path, why: &str) -> Error {
    Error::new(
        ErrorKind::Store,
        format!(
            "the record of instance {name} is unreadable ({why}): {}",
            path.join("record.json").display()
        ),
    )
}

/// Proof of the shared op lock of one instance (`run`, `ssh`, `cp`). It cannot change or remove anything.
pub(crate) struct OpShared {
    _lock: FileLock,
}

/// An op lock of one instance, shared or exclusive: what reading the instance's machine and keys needs.
pub(crate) trait OpHeld {}

impl OpHeld for OpExclusive {}

impl OpHeld for OpShared {}

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

    /// The instance's disk, a copy-on-write overlay of its layer.
    pub(crate) fn disk(&self) -> PathBuf {
        self.path.join("disk.img")
    }

    /// What the instance's machine writes to its serial port.
    pub(crate) fn serial_log(&self) -> PathBuf {
        self.path.join("serial.log")
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
            RecordRead::Corrupt(why) => Err(unreadable_record(&self.name, &self.path, &why)),
        }
    }

    /// The owner marker on disk, whichever instance it names.
    pub(crate) fn marker(&self) -> Result<MarkerRead> {
        self.locks.read_marker(&self.path)
    }

    /// Writes the owner marker `marker` into this instance's directory, after `before_write` ran. The directory cannot
    /// be renamed meanwhile, and it must still be this handle's instance: `NotFound` if it is gone or another one,
    /// `Store` if its record is unreadable; nothing is written either way.
    pub(super) fn write_marker(&self, marker: &Marker, before_write: &mut dyn FnMut()) -> Result<()> {
        self.in_this_instance(|| {
            before_write();
            write_marker(&self.path, marker)
        })?
        .ok_or_else(|| self.gone())
    }

    /// [`in_instance`] for this handle's instance.
    fn in_this_instance<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<Option<T>> {
        in_instance(&self.locks, &self.name, &self.path, &self.id, f)
    }

    fn what(&self) -> String {
        format!("another devvm command on instance {}", self.name)
    }

    /// What a wait for an op lock says it waits for when the exclusive holder is recorded and still runs:
    /// `<program> (PID <pid>) on instance <name>`. `None` names no one: no record, an unusable one, or one whose process
    /// has exited, is not this one or cannot be assessed. A record that cannot be read is a debug note.
    fn holder_text(&self) -> Option<String> {
        let read = self.in_this_instance(|| read_holder(&self.path));
        let holder = match read {
            Ok(Some(Some(holder))) => holder,
            Ok(_) => return None,
            Err(e) => {
                self.progress.debug(&format!(
                    "cannot read who holds instance {}: {}",
                    self.name,
                    e.message()
                ));
                return None;
            }
        };
        let process = ProcessId::try_from(&holder.process).ok()?;
        matches!(process.is_alive(), Liveness::Alive)
            .then(|| format!("{} (PID {}) on instance {}", holder.program, process.pid(), self.name))
    }

    /// Writes this process as the holder, once the exclusive lock is held. A record that cannot be written is a warning:
    /// the process goes on, and a command that waits behind it names no one.
    fn record_holder(&self) {
        let written = Holder::current()
            .and_then(|holder| self.in_this_instance(|| write_holder(&self.path, &holder)));
        if let Err(e) = written {
            self.progress.warn(&format!(
                "instance {}: cannot record the holder ({})",
                self.name,
                e.message()
            ));
        }
    }

    fn exclusive(&self, lock: FileLock) -> OpExclusive {
        OpExclusive {
            id: self.id,
            name: self.name.clone(),
            path: self.path.clone(),
            locks: self.locks.clone(),
            progress: self.progress.clone(),
            _lock: lock,
        }
    }

    /// Waits for the exclusive op lock (`start`, `stop`, `rm`) and records this process as its holder. `NotFound` if the
    /// instance went away meanwhile.
    pub(crate) fn op_exclusive(&self) -> Result<OpExclusive> {
        let lock = self
            .locks
            .op_exclusive(&self.id, &self.what(), &*self.progress, &|| self.holder_text())?;
        self.record()?;
        let op = self.exclusive(lock);
        self.record_holder();
        Ok(op)
    }

    /// Waits for the shared op lock (`run`, `ssh`, `cp`). `NotFound` if the instance went away meanwhile.
    pub(crate) fn op_shared(&self) -> Result<OpShared> {
        let lock = self
            .locks
            .op_shared(&self.id, &self.what(), &*self.progress, &|| self.holder_text())?;
        self.record()?;
        Ok(OpShared { _lock: lock })
    }

    /// The exclusive op lock if nobody holds it, else `None`. `NotFound` if the instance is gone. The holder is not
    /// recorded: a wait behind it names no one.
    pub(crate) fn try_op_exclusive(&self) -> Result<Option<OpExclusive>> {
        match self.locks.try_op_exclusive(&self.id)? {
            Probe::Free(lock) => {
                self.record()?;
                Ok(Some(self.exclusive(lock)))
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

    /// `backend.find(machine)` for this instance's machine. A `process-record` error gets the step that gets out: the
    /// VM cannot be told to run or not, so the person stops it by hand and clears the `machine` field of the record.
    pub(crate) fn find_machine(
        &self,
        backend: &dyn Backend,
        machine: &MachineRecord,
    ) -> Result<Option<Box<dyn Machine>>> {
        backend.find(machine).map_err(|e| {
            if e.kind() != ErrorKind::ProcessRecord {
                return e;
            }
            e.with_step(format_args!(
                "stop the VM by hand if it still runs, then clear the `machine` field in {}",
                self.path.join(RECORD_FILE).display()
            ))
        })
    }

    /// The machine named by `machine` is dead: deletes what it left outside the instance directory (a file that
    /// cannot be deleted goes to `warn`) and clears the record that names it. Call it only once the machine is proven
    /// dead, by a kill or by the backend not finding it.
    pub(crate) fn retire_machine(
        &self,
        op: &OpExclusive,
        machine: &MachineRecord,
        host: &dyn Host,
        warn: &mut dyn FnMut(String),
    ) -> Result<()> {
        machine.remove_leftovers(host, warn);
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
            Ok(Err(e)) => self.progress.warn(&format!(
                "instance {} is removed, but {} could not be deleted ({e}); the next devvm command retries",
                self.name,
                dead.display()
            )),
            Ok(Ok(())) => {}
            Err(e) => self.progress.warn(&format!(
                "instance {} is removed, but {} could not be deleted ({}); the next devvm command retries",
                self.name,
                dead.display(),
                e.message()
            )),
        }
        // The instance is gone either way; leftover lock files go in the next lock sweep.
        if let Err(e) = self.locks.remove(&self.id) {
            self.progress.warn(&format!(
                "instance {} is removed, but its lock files could not be deleted ({}); the next devvm command retries",
                self.name,
                e.message()
            ));
        }
        Ok(())
    }
}
