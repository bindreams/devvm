//! The ways something other than the holder of a file can make its name lead to another file.

use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub enum NameChange {
    /// The file is unlinked and a new one made at the name.
    UnlinkThenRecreate,
    /// The file is renamed away and a new one made at the name.
    RenameAwayThenRecreate,
    /// Another file is renamed over the name.
    Replace,
}

impl NameChange {
    pub const ALL: [NameChange; 3] = [
        NameChange::UnlinkThenRecreate,
        NameChange::RenameAwayThenRecreate,
        NameChange::Replace,
    ];

    /// Changes what the file `path` names. The file keeps its open handles, and a file stays at `path`.
    pub fn apply(self, path: &Path) {
        let beside = |suffix: &str| {
            let mut name = path.file_name().unwrap().to_owned();
            name.push(suffix);
            path.with_file_name(name)
        };
        match self {
            NameChange::UnlinkThenRecreate => {
                std::fs::remove_file(path).unwrap();
                std::fs::write(path, b"").unwrap();
            }
            NameChange::RenameAwayThenRecreate => {
                std::fs::rename(path, beside(".moved")).unwrap();
                std::fs::write(path, b"").unwrap();
            }
            NameChange::Replace => {
                let other = beside(".other");
                std::fs::write(&other, b"").unwrap();
                std::fs::rename(&other, path).unwrap();
            }
        }
    }
}
