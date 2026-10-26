//! The instance records as the layer store's reachability: every instance keeps its whole layer chain alive.

use std::fs::FileType;
use std::path::{Path, PathBuf};

use crate::instance::record::{InstanceRecord, RecordRead};
use crate::instance::registry::Registry;
use crate::store::layers::{LayerRef, Reachability};
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "reach_tests.rs"]
mod reach_tests;

impl Reachability for Registry {
    /// The chain of every placed instance, running or stopped, owned or not, root first, and of every quarantined one.
    ///
    /// Only reads `record.json` files and takes no lock but the leaf `instances.dir` lock (see `Locks`): collection
    /// calls this while holding the exclusive `gc.lock`, and a creator waiting on that lock may hold the very locks a
    /// wait here would need, but no holder of `instances.dir` ever waits for anything. A directory whose record is
    /// unreadable, or an entry that cannot be examined, fails the call naming it, so collection stops before it could
    /// delete something that instance needs. Dot-directories (`.new-*`, `.dead-*`) and placed directories without a
    /// record (one being removed) hold no instance.
    ///
    /// Quarantine never deletes an instance's disk, which sits on its chain, so a quarantined instance keeps the chain
    /// alive too. Its record cannot be missing: nothing is being removed there.
    fn chains(&self) -> Result<Vec<Vec<LayerRef>>> {
        let mut chains = Vec::new();
        for path in self.directories(self.root(), true)? {
            match self.locks().read_record(&path)? {
                RecordRead::Valid(record) => chains.push(record.chain),
                RecordRead::Absent => {}
                RecordRead::Corrupt(why) => return Err(unreadable(&path, &why)),
            }
        }
        for path in self.directories(&self.quarantine_root().join("instances"), false)? {
            match InstanceRecord::read(&path)? {
                RecordRead::Valid(record) => chains.push(record.chain),
                RecordRead::Absent => return Err(unreadable(&path, "record.json is missing")),
                RecordRead::Corrupt(why) => return Err(unreadable(&path, &why)),
            }
        }
        Ok(chains)
    }
}

impl Registry {
    /// The subdirectories of `root` (none when it does not exist), skipping dot-directories if `skip_hidden`.
    fn directories(&self, root: &Path, skip_hidden: bool) -> Result<Vec<PathBuf>> {
        let list_failed = |e| Error::io(ErrorKind::Io, format!("cannot list {}", root.display()), e);
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(list_failed(e)),
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(list_failed)?;
            if skip_hidden && entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if entry_is_dir(&entry.path(), entry.file_type())? {
                found.push(entry.path());
            }
        }
        Ok(found)
    }
}

/// Whether the entry at `path` is a directory, given the outcome of asking for its type. An entry whose type cannot be
/// read is an error, not a skip: it might be an instance whose chain is needed.
pub(super) fn entry_is_dir(path: &Path, kind: std::io::Result<FileType>) -> Result<bool> {
    kind.map(|kind| kind.is_dir()).map_err(|e| {
        Error::io(
            ErrorKind::Io,
            format!(
                "cannot collect: cannot tell whether {} is an instance directory; nothing was deleted",
                path.display()
            ),
            e,
        )
    })
}

fn unreadable(path: &Path, why: &str) -> Error {
    Error::new(
        ErrorKind::Store,
        format!(
            "cannot collect: the record of an instance is unreadable ({why}): {}; nothing was deleted",
            path.join("record.json").display()
        ),
    )
}
