//! The reaper: removes instances whose owner is gone, finishes half-done creations and removals, and moves what it
//! cannot read to quarantine. Any devvm invocation may run it; it never fails, every problem is a warning that the next
//! invocation retries.
//!
//! Rules, per entry of `instances/`:
//!
//! 1. `.dead-<id>`: delete.
//! 2. `.new-<id>`: delete, but only once the creator's owner lock is free (or its lock file is gone).
//! 3. A placed directory:
//!    - a record or marker from a newer devvm, or one that cannot be read for another reason: warn and leave it alone;
//!    - a record or marker that is absent, torn or hand-edited: move the directory to quarantine, under the owner and
//!      op locks when the instance id is known;
//!    - otherwise: take the owner lock without waiting (held: skip), re-read the marker (`none`: skip), take the op lock
//!      without waiting (busy: skip), kill the recorded VM through its identity, then remove the instance.
//! 4. Lock files whose owner lock is free and that no directory (placed, `.new` or `.dead`) belongs to are deleted.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::contain::VmProcess;
use crate::instance::dir::remove_tree;
use crate::instance::locks::Probe;
use crate::instance::name::InstanceName;
use crate::instance::quarantine::{Placed, classify};
use crate::instance::record::{InstanceId, InstanceRecord, MarkerRead, Owner, RecordRead};
use crate::instance::registry::Registry;
use crate::{Error, ErrorKind};

#[cfg(test)]
#[path = "reaper_tests.rs"]
mod reaper_tests;

#[cfg(all(test, windows))]
#[path = "reaper_windows_tests.rs"]
mod reaper_windows_tests;

/// What one pass did.
#[derive(Debug, Default)]
pub(crate) struct ReapReport {
    /// Names of the instances removed.
    pub reaped: Vec<String>,
    /// Where each unreadable instance directory was moved.
    pub quarantined: Vec<PathBuf>,
    /// Everything that did not work; also sent to the registry's progress.
    pub warnings: Vec<String>,
}

impl ReapReport {
    pub(super) fn warn(&mut self, registry: &Registry, text: String) {
        registry.progress().message(&text);
        self.warnings.push(text);
    }
}

impl Registry {
    /// One reaper pass. Never fails.
    pub(crate) fn reap(&self) -> ReapReport {
        let mut report = ReapReport::default();
        match self.entries() {
            Ok(entries) => {
                for (name, path) in entries {
                    if name.starts_with(".dead-") {
                        self.reap_dead(&name, &path, &mut report);
                    } else if let Some(id) = name.strip_prefix(".new-") {
                        self.reap_new(id, &name, &path, &mut report);
                    } else if !name.starts_with('.') {
                        self.reap_placed(&name, &path, &mut report);
                    }
                }
            }
            Err(e) => report.warn(self, format!("cannot list instances: {}", e.message())),
        }
        self.sweep_locks(&mut report);
        report
    }

    /// The directories under `instances/`, by name; empty when there is no such directory.
    fn entries(&self) -> crate::Result<Vec<(String, PathBuf)>> {
        let entries = match std::fs::read_dir(self.root()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(Error::io(
                    ErrorKind::Io,
                    format!("cannot list {}", self.root().display()),
                    e,
                ));
            }
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|e| Error::io(ErrorKind::Io, format!("cannot list {}", self.root().display()), e))?;
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            if let Ok(name) = entry.file_name().into_string() {
                found.push((name, entry.path()));
            }
        }
        found.sort();
        Ok(found)
    }

    // Leftovers -------------------------------------------------------------------------------------------------------

    /// Deletes a `.dead-<id>` directory unless a deletion is already running: two deleters of one tree make each other
    /// fail on Windows.
    fn reap_dead(&self, name: &str, path: &Path, report: &mut ReapReport) {
        if let Err(e) = self.locks().deleting(|| self.remove_leftover(name, path, report)) {
            report.warn(self, format!("cannot delete {name}: {}", e.message()));
        }
    }

    fn remove_leftover(&self, name: &str, path: &Path, report: &mut ReapReport) {
        match remove_tree(path) {
            Ok(()) => {}
            Err(e) => report.warn(
                self,
                format!("cannot delete {name} ({e}); the next devvm command retries"),
            ),
        }
    }

    fn reap_new(&self, id: &str, name: &str, path: &Path, report: &mut ReapReport) {
        let Ok(id) = id.parse::<InstanceId>() else { return };
        // Held means the creator is alive and still building. A missing lock file means nobody can be building.
        let _creator_gone = match self.locks().try_owner(&id) {
            Ok(Probe::Held) => return,
            Ok(Probe::Free(lock)) => Some(lock),
            Ok(Probe::Gone) => None,
            Err(e) => {
                report.warn(self, format!("cannot check the owner of {name}: {}", e.message()));
                return;
            }
        };
        self.remove_leftover(name, path, report);
        // The creator may have placed the instance after the listing and let go of its owner lock: the lock files go
        // only when, with the owner lock held, no placed, `.new` or `.dead` directory belongs to the id.
        let (present, unattributed) = self.present_ids();
        if !unattributed && !present.contains(&id) {
            self.locks().remove(&id);
        }
    }

    // Placed instances ------------------------------------------------------------------------------------------------

    fn reap_placed(&self, name: &str, path: &Path, report: &mut ReapReport) {
        let (record, marker) = (self.locks().read_record(path), self.locks().read_marker(path));
        // A newer devvm's instance, or a file we cannot read for another reason, is never trusted and never touched.
        if let Some(error) = record.as_ref().err().or(marker.as_ref().err()) {
            report.warn(self, format!("leaving instance {name} alone: {}", error.message()));
            return;
        }
        let (Ok(record), Ok(marker)) = (record, marker) else {
            return;
        };
        match classify(name, record, marker) {
            Placed::Readable(record) => self.reap_owned(name, path, *record, report),
            Placed::Unreadable { why, id } => self.quarantine(name, path, id, &why, report),
        }
    }

    /// A readable instance: remove it if its owner is gone.
    fn reap_owned(&self, name: &str, path: &Path, record: InstanceRecord, report: &mut ReapReport) {
        let id = record.id;
        // The record names this id and the directory exists, so lock files that are missing were deleted from outside.
        let owner_lock = match self.locks().try_owner_recreating(&id) {
            Ok(Probe::Free(lock)) => lock,
            // Held: the owner is alive.
            Ok(Probe::Held | Probe::Gone) => return,
            Err(e) => {
                report.warn(
                    self,
                    format!("cannot check the owner of instance {name}: {}", e.message()),
                );
                return;
            }
        };
        // The marker is read under the lock: an ownerless instance may have been flipped since the first read.
        match self.locks().read_marker(path) {
            Ok(MarkerRead::Valid(marker)) if marker.owner == Owner::Process && marker.id == id => {}
            Ok(_) => return,
            Err(e) => {
                report.warn(self, format!("leaving instance {name} alone: {}", e.message()));
                return;
            }
        }
        let Ok(instance_name) = InstanceName::parse(name) else {
            return;
        };
        let dir = self.handle(id, instance_name);
        let op = match dir.try_op_exclusive() {
            Ok(Some(op)) => op,
            // Busy: another command is working on it; this is not an error.
            Ok(None) => return,
            Err(e) if e.kind() == ErrorKind::NotFound => return,
            Err(e) => {
                report.warn(self, format!("cannot lock instance {name}: {}", e.message()));
                return;
            }
        };
        let record = match dir.record() {
            Ok(record) => record,
            Err(e) => {
                report.warn(self, format!("cannot read instance {name}: {}", e.message()));
                return;
            }
        };
        if let Some(machine) = &record.machine {
            match VmProcess::attach(&machine.process) {
                Ok(Some(mut vm)) => {
                    if let Err(e) = vm.kill() {
                        report.warn(
                            self,
                            format!(
                                "cannot stop the VM of instance {name}: {}; it is left for the next devvm command",
                                e.message()
                            ),
                        );
                        return;
                    }
                }
                // Not running, or the pid now belongs to another process: nothing of ours to kill.
                Ok(None) => {}
                Err(e) => {
                    report.warn(
                        self,
                        format!(
                            "cannot check the VM of instance {name}: {}; it is left for the next devvm command",
                            e.message()
                        ),
                    );
                    return;
                }
            }
            if let Err(e) = dir.update(&op, &mut |record| record.machine = None) {
                report.warn(self, format!("cannot update instance {name}: {}", e.message()));
            }
        }
        match dir.destroy(op) {
            Ok(()) => report.reaped.push(name.to_owned()),
            Err(e) => report.warn(self, format!("cannot remove instance {name}: {}", e.message())),
        }
        drop(owner_lock);
    }

    // Lock files ------------------------------------------------------------------------------------------------------

    /// Ids that a directory under `instances/` belongs to, and whether some placed directory could not be attributed
    /// to an id (then no lock file may be called an orphan).
    fn present_ids(&self) -> (HashSet<InstanceId>, bool) {
        let mut present = HashSet::new();
        let mut unattributed = false;
        let Ok(entries) = self.entries() else {
            return (present, true);
        };
        for (name, path) in entries {
            let hidden = name.strip_prefix(".new-").or_else(|| name.strip_prefix(".dead-"));
            if let Some(text) = hidden {
                match text.parse::<InstanceId>() {
                    Ok(id) => {
                        present.insert(id);
                    }
                    Err(_) => unattributed = true,
                }
            } else if !name.starts_with('.') {
                let from_record = match self.locks().read_record(&path) {
                    Ok(RecordRead::Valid(record)) => Some(record.id),
                    _ => None,
                };
                let from_marker = match self.locks().read_marker(&path) {
                    Ok(MarkerRead::Valid(marker)) => Some(marker.id),
                    _ => None,
                };
                match from_record.or(from_marker) {
                    Some(id) => {
                        present.insert(id);
                    }
                    None => unattributed = true,
                }
            }
        }
        (present, unattributed)
    }

    /// Deletes the lock files of instances that no longer exist. The directory listing is taken after the owner lock,
    /// so a creator that gets the lock first is seen.
    fn sweep_locks(&self, report: &mut ReapReport) {
        let ids = match self.locks().ids() {
            Ok(ids) => ids,
            Err(e) => {
                report.warn(self, format!("cannot list lock files: {}", e.message()));
                return;
            }
        };
        for id in ids {
            let Ok(Probe::Free(lock)) = self.locks().try_owner(&id) else {
                continue;
            };
            let (present, unattributed) = self.present_ids();
            if !unattributed && !present.contains(&id) {
                self.locks().remove(&id);
            }
            drop(lock);
        }
    }
}
