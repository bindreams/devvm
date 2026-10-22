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

// Fakes ===============================================================================================================

/// A clock the test sets.
pub struct FakeClock {
    now: std::sync::Mutex<std::time::SystemTime>,
    scripts: std::sync::Mutex<std::collections::HashMap<std::thread::ThreadId, (std::time::SystemTime, bool)>>,
}

impl FakeClock {
    pub fn new(now: std::time::SystemTime) -> FakeClock {
        FakeClock {
            now: std::sync::Mutex::new(now),
            scripts: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn set(&self, now: std::time::SystemTime) {
        *self.now.lock().unwrap() = now;
    }

    /// Makes the calling thread's reads return the shared time once, then `later` from the second read on. This
    /// models time passing between two steps of one thread without depending on how other threads interleave.
    pub fn script_this_thread(&self, later: std::time::SystemTime) {
        self.scripts
            .lock()
            .unwrap()
            .insert(std::thread::current().id(), (later, false));
    }
}

impl crate::store::layers::Clock for FakeClock {
    fn now(&self) -> std::time::SystemTime {
        if let Some((later, started)) = self.scripts.lock().unwrap().get_mut(&std::thread::current().id()) {
            if *started {
                return *later;
            }
            *started = true;
        }
        *self.now.lock().unwrap()
    }
}

/// A progress sink the test reads back.
#[derive(Default)]
pub struct MemoryProgress(std::sync::Mutex<Vec<String>>);

impl MemoryProgress {
    pub fn messages(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl crate::exit::Progress for MemoryProgress {
    fn message(&self, text: &str) {
        self.0.lock().unwrap().push(text.to_owned());
    }
}
