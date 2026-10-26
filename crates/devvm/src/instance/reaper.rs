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

use crate::instance::dir::remove_tree;
use crate::instance::locks::Probe;
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

/// The entries of `instances/` as the reaper sees them.
#[derive(Default)]
pub(super) struct Entries {
    /// Name and path of each directory.
    pub dirs: Vec<(String, PathBuf)>,
    /// Entries whose type could not be read. They may be instances, so no lock file may be called an orphan while
    /// there is one.
    pub unreadable: Vec<(PathBuf, std::io::Error)>,
}

impl Entries {
    /// Sorts an entry by what `kind`, the result of asking for its type, says it is. A directory with a name that is
    /// not text cannot be an instance's, since instance names and ids are text.
    pub(super) fn push(&mut self, name: std::ffi::OsString, path: PathBuf, kind: std::io::Result<std::fs::FileType>) {
        match kind {
            Ok(kind) if kind.is_dir() => {
                if let Ok(name) = name.into_string() {
                    self.dirs.push((name, path));
                }
            }
            Ok(_) => {}
            Err(e) => self.unreadable.push((path, e)),
        }
    }
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
            Ok(entries) => self.reap_listed(entries, &mut report),
            Err(e) => report.warn(self, format!("cannot list instances: {}", e.message())),
        }
        self.sweep_locks(&mut report);
        report
    }

    /// The reaper's rules for each of `entries`.
    pub(super) fn reap_listed(&self, entries: Entries, report: &mut ReapReport) {
        for (path, error) in &entries.unreadable {
            report.warn(
                self,
                format!("cannot tell what {} is ({error}); it is left alone", path.display()),
            );
        }
        for (name, path) in entries.dirs {
            if name.starts_with(".dead-") {
                self.reap_dead(&name, &path, report);
            } else if let Some(id) = name.strip_prefix(".new-") {
                self.reap_new(id, &name, &path, report);
            } else if !name.starts_with('.') {
                self.reap_placed(&name, &path, report);
            }
        }
    }

    /// The directories under `instances/`; empty when there is no such directory.
    fn entries(&self) -> crate::Result<Entries> {
        let entries = match std::fs::read_dir(self.root()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Entries::default()),
            Err(e) => {
                return Err(Error::io(
                    ErrorKind::Io,
                    format!("cannot list {}", self.root().display()),
                    e,
                ));
            }
        };
        let mut found = Entries::default();
        for entry in entries {
            let entry =
                entry.map_err(|e| Error::io(ErrorKind::Io, format!("cannot list {}", self.root().display()), e))?;
            found.push(entry.file_name(), entry.path(), entry.file_type());
        }
        found.dirs.sort();
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
        let Ok(id) = id.parse::<InstanceId>() else {
            report.warn(
                self,
                format!("leaving {name} alone: the part after `.new-` is not an instance id"),
            );
            return;
        };
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
        if !unattributed
            && !present.contains(&id)
            && let Err(e) = self.locks().remove(&id)
        {
            report.warn(self, format!("cannot delete the lock files of {name}: {}", e.message()));
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
            Ok(Probe::Held) => return,
            Ok(Probe::Gone) => unreachable!("try_owner_recreating puts back a missing lock file"),
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
        let dir = self.handle(id, record.name.clone());
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
            let Some(backend) = self.backend() else {
                report.warn(
                    self,
                    format!(
                        "cannot stop the VM of instance {name}: this build of devvm has no VM backend; it is left for \
                         the next devvm command"
                    ),
                );
                return;
            };
            match backend.find(machine) {
                Ok(Some(mut running)) => {
                    if let Err(e) = running.kill() {
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
            // The machine is dead, so what it left outside the instance directory can go.
            let retired = dir.retire_machine(&op, machine, &mut |text| {
                report.warn(self, format!("instance {name}: {text}"))
            });
            if let Err(e) = retired {
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
        self.present_ids_with(&mut || {})
    }

    /// `present_ids` with `listing` run once for each read of the instance directories.
    fn present_ids_with(&self, listing: &mut dyn FnMut()) -> (HashSet<InstanceId>, bool) {
        listing();
        match self.entries() {
            Ok(entries) => self.attribute(entries),
            Err(_) => (HashSet::new(), true),
        }
    }

    /// The ids that a directory of `entries` belongs to, and whether some entry could not be attributed.
    pub(super) fn attribute(&self, entries: Entries) -> (HashSet<InstanceId>, bool) {
        let mut present = HashSet::new();
        let mut unattributed = !entries.unreadable.is_empty();
        for (name, path) in entries.dirs {
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

    /// Deletes the lock files of instances that no longer exist. It takes every free owner lock first and lists the
    /// instance directories once afterwards, so a creator that held a lock and let go of it before this pass took it
    /// has placed its instance, and the listing sees it.
    fn sweep_locks(&self, report: &mut ReapReport) {
        self.sweep_locks_with(report, &mut || {});
    }

    fn sweep_locks_with(&self, report: &mut ReapReport, listing: &mut dyn FnMut()) {
        let ids = match self.locks().ids() {
            Ok(ids) => ids,
            Err(e) => {
                report.warn(self, format!("cannot list lock files: {}", e.message()));
                return;
            }
        };
        let free: Vec<_> = ids
            .into_iter()
            .filter_map(|id| match self.locks().try_owner(&id) {
                Ok(Probe::Free(lock)) => Some((id, lock)),
                _ => None,
            })
            .collect();
        if free.is_empty() {
            return;
        }
        let (present, unattributed) = self.present_ids_with(listing);
        if !unattributed {
            for (id, _lock) in &free {
                if !present.contains(id)
                    && let Err(e) = self.locks().remove(id)
                {
                    report.warn(self, format!("cannot delete the lock files of {id}: {}", e.message()));
                }
            }
        }
    }
}
