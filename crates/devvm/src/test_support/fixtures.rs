//! Skuld fixtures shared by the unit tests.

use std::ops::Deref;
use std::path::{Path, PathBuf};

/// A fresh directory named `Anna Ж` (a space and a non-ASCII character), removed on drop.
pub struct AnnaHome {
    path: PathBuf,
    _tmp: skuld::TempDir,
}

impl Deref for AnnaHome {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

/// Every test that touches the file system works under this directory, so a path with a space and a non-ASCII
/// character is the normal case, not an edge case.
#[skuld::fixture(deref)]
pub fn anna_home() -> Result<AnnaHome, String> {
    let tmp = skuld::TempDir::new().map_err(|e| format!("cannot create a temp dir: {e}"))?;
    let path = tmp.join("Anna Ж");
    std::fs::create_dir(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    Ok(AnnaHome { path, _tmp: tmp })
}
