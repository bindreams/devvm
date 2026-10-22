//! The instance records as the layer store's reachability: every instance keeps its whole layer chain alive.

use crate::instance::record::{InstanceRecord, RecordRead};
use crate::instance::registry::Registry;
use crate::store::layers::{LayerRef, Reachability};
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "reach_tests.rs"]
mod reach_tests;

impl Reachability for Registry {
    /// The chain of every placed instance, running or stopped, owned or not, root first.
    ///
    /// Only reads `record.json` files and takes no lock: collection calls this while holding the exclusive `gc.lock`,
    /// and a creator waiting on that lock may hold the very locks a wait here would need. A directory whose record is
    /// unreadable fails the call naming it, so collection stops before it could delete something that instance needs.
    /// Dot-directories (`.new-*`, `.dead-*`) and directories without a record (one being removed) hold no instance.
    fn chains(&self) -> Result<Vec<Vec<LayerRef>>> {
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
        let mut chains = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|e| Error::io(ErrorKind::Io, format!("cannot list {}", self.root().display()), e))?;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let path = entry.path();
            match InstanceRecord::read(&path)? {
                RecordRead::Valid(record) => chains.push(record.chain),
                RecordRead::Absent => {}
                RecordRead::Corrupt(why) => {
                    return Err(Error::new(
                        ErrorKind::Store,
                        format!(
                            "cannot collect: the record of an instance is unreadable ({why}): {}; nothing was deleted",
                            path.join("record.json").display()
                        ),
                    ));
                }
            }
        }
        Ok(chains)
    }
}
