//! Quarantine: moving an instance directory that cannot be read out of `instances/`, never deleting it.

use std::path::{Path, PathBuf};

use crate::instance::locks::Probe;
use crate::instance::reaper::ReapReport;
use crate::instance::record::{InstanceId, InstanceRecord, MarkerRead, RecordRead, read_marker};
use crate::instance::registry::Registry;
use crate::store::lock::FileLock;

#[cfg(test)]
#[path = "quarantine_tests.rs"]
mod quarantine_tests;

/// What a placed directory is, judged from its two files.
pub(super) enum Placed {
    /// A complete instance of this directory name.
    Readable(Box<InstanceRecord>),
    /// Not a valid instance (and not from a newer devvm). `id` is the instance one of the files claims to be.
    Unreadable { why: String, id: Option<InstanceId> },
}

/// A record or marker that is absent, torn, hand-edited, for another instance, or for another directory name is
/// unreadable. A directory is only named by its instance's record, so a moved or copied one is not trusted.
pub(super) fn classify(name: &str, record: RecordRead, marker: MarkerRead) -> Placed {
    let (record, marker) = match (record, marker) {
        (RecordRead::Valid(record), MarkerRead::Valid(marker))
            if record.id == marker.id && record.name.as_str() == name =>
        {
            return Placed::Readable(record);
        }
        pair => pair,
    };
    let why = match (&record, &marker) {
        (RecordRead::Corrupt(why), _) => format!("record.json: {why}"),
        (RecordRead::Absent, _) => "record.json is missing".to_owned(),
        (_, MarkerRead::Corrupt(why)) => format!("owner.json: {why}"),
        (_, MarkerRead::Absent) => "owner.json is missing".to_owned(),
        (RecordRead::Valid(record), MarkerRead::Valid(marker)) if record.id != marker.id => {
            "record.json and owner.json name different instances".to_owned()
        }
        (RecordRead::Valid(record), _) => format!("record.json names instance {}, not {name}", record.name),
    };
    let id = match (&record, &marker) {
        (RecordRead::Valid(record), _) => Some(record.id),
        (_, MarkerRead::Valid(marker)) => Some(marker.id),
        _ => None,
    };
    Placed::Unreadable { why, id }
}

/// What the move did, decided and done while no one reads inside any instance directory.
enum Moved {
    Done,
    /// The path is gone: the instance was removed since the verdict.
    Vanished,
    /// The directory is not what the verdict was about (valid now, or another instance): left alone.
    Changed,
    /// A file from a newer devvm: left alone.
    Newer(String),
    Failed(std::io::Error),
}

impl Registry {
    /// Moves an unreadable instance directory to `<state>/quarantine/instances/<name>-<random>`; never deletes it.
    ///
    /// `id` is the instance the verdict named, if any; when known the move happens under both its locks, so a live
    /// owner or a running command stops it. The verdict was taken without any lock, so it is checked again at the
    /// moment of the move, while no devvm command reads or renames an instance directory (nothing can place or remove
    /// the directory in between): the move happens only if the directory at `path` is still unreadable and still names
    /// the same `id`. A path that is gone is success.
    pub(super) fn quarantine(
        &self,
        name: &str,
        path: &Path,
        id: Option<InstanceId>,
        why: &str,
        report: &mut ReapReport,
    ) {
        let mut held: Vec<FileLock> = Vec::new();
        if let Some(id) = id {
            let probes = [self.locks().try_owner(&id), self.locks().try_op_exclusive(&id)];
            for probe in probes {
                match probe {
                    Ok(Probe::Free(lock)) => held.push(lock),
                    Ok(Probe::Held) => return,
                    // No lock file: nobody holds the lock.
                    Ok(Probe::Gone) => {}
                    Err(e) => {
                        report.warn(self, format!("cannot lock unreadable instance {name}: {}", e.message()));
                        return;
                    }
                }
            }
        }
        let mut suffix = [0u8; 8];
        getrandom::fill(&mut suffix).expect("the OS random number generator is available");
        let target = self
            .quarantine_root()
            .join("instances")
            .join(format!("{name}-{}", hex::encode(suffix)));
        match self
            .locks()
            .renaming(&mut || {}, || move_if_unchanged(name, path, id, &target))
        {
            Ok(Moved::Done) => {
                self.progress().message(&format!(
                    "instance {name} is unreadable ({why}); moved it to {} (nothing was deleted)",
                    target.display()
                ));
                report.quarantined.push(target);
            }
            Ok(Moved::Vanished | Moved::Changed) => {}
            Ok(Moved::Newer(text)) => report.warn(self, format!("leaving instance {name} alone: {text}")),
            Ok(Moved::Failed(e)) => report.warn(
                self,
                format!("cannot move unreadable instance {name} to quarantine ({e}); the next devvm command retries"),
            ),
            Err(e) => report.warn(self, format!("cannot lock unreadable instance {name}: {}", e.message())),
        }
        drop(held);
    }
}

/// Runs inside `Locks::renaming`: reads the files directly, takes no lock.
fn move_if_unchanged(name: &str, path: &Path, id: Option<InstanceId>, target: &PathBuf) -> Moved {
    if !path.exists() {
        return Moved::Vanished;
    }
    let (record, marker) = match (InstanceRecord::read(path), read_marker(path)) {
        (Ok(record), Ok(marker)) => (record, marker),
        (Err(e), _) | (_, Err(e)) => return Moved::Newer(e.message().to_owned()),
    };
    match classify(name, record, marker) {
        Placed::Unreadable { id: now, .. } if now == id => {}
        _ => return Moved::Changed,
    }
    let parent = target.parent().expect("a quarantine target has a parent");
    match std::fs::create_dir_all(parent).and_then(|()| std::fs::rename(path, target)) {
        Ok(()) => Moved::Done,
        // Gone since the check above: the instance was removed by a process outside devvm's locks.
        Err(_) if !path.exists() => Moved::Vanished,
        Err(e) => Moved::Failed(e),
    }
}
