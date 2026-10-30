//! Skuld fixtures shared by the unit tests.

use std::ops::Deref;
use std::path::{Path, PathBuf};

#[path = "fixtures_tests.rs"]
mod fixtures_tests;

/// The name of the Unicode test directory. Each character is a case: `\u{e9}` is in code page 1252, `\u{416}` is
/// not (1251 has it), `\u{4e16}` is in no single-byte code page, `\u{1f600}` is a UTF-16 surrogate pair, and the
/// spaces split arguments. Spelled with escapes so no editor can normalise `\u{e9}` to `e` + U+0301.
pub const UNICODE_DIR: &str = "unicode-\u{e9} \u{416} \u{4e16} \u{1f600}";

/// A fresh directory named [`UNICODE_DIR`], removed on drop.
pub struct UnicodeHome {
    path: PathBuf,
    _tmp: skuld::TempDir,
}

impl Deref for UnicodeHome {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

/// Every test that touches the file system works under this directory, so a path with spaces, non-ASCII characters
/// and a surrogate pair is the normal case, not an edge case.
#[skuld::fixture(deref)]
pub fn unicode_home() -> Result<UnicodeHome, String> {
    let tmp = skuld::TempDir::new().map_err(|e| format!("cannot create a temp dir: {e}"))?;
    let path = tmp.join(UNICODE_DIR);
    std::fs::create_dir(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    Ok(UnicodeHome { path, _tmp: tmp })
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

/// A progress sink that sends every message down a channel, so a test can wait for "waiting for ..." as an event.
pub struct ChannelProgress(std::sync::Mutex<std::sync::mpsc::Sender<String>>);

impl ChannelProgress {
    pub fn new() -> (std::sync::Arc<ChannelProgress>, std::sync::mpsc::Receiver<String>) {
        let (sender, receiver) = std::sync::mpsc::channel();
        (
            std::sync::Arc::new(ChannelProgress(std::sync::Mutex::new(sender))),
            receiver,
        )
    }
}

impl crate::exit::Progress for ChannelProgress {
    fn message(&self, text: &str) {
        let _ = self.0.lock().unwrap().send(text.to_owned());
    }
}
