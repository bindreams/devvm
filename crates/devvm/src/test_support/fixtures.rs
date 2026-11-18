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

/// A base build's context for a root that is no ISO: the directories under `home`, a build directory `home/build`, the
/// system clock and no resources set. Set `iso` for an ISO root.
pub fn build_ctx(home: &Path, arch: crate::Arch) -> crate::guest::BuildCtx {
    crate::guest::BuildCtx {
        dirs: crate::Dirs::under(home),
        arch,
        progress: std::sync::Arc::new(crate::progress::NullProgress),
        clock: std::sync::Arc::new(crate::store::layers::SystemClock),
        resources: crate::recipe::Resources::default(),
        work: home.join("build"),
        iso: std::sync::Mutex::default(),
    }
}

/// The installer ISO fixture `name`, which `iso/fixtures/make_fixtures.py` makes.
pub fn iso_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/iso/fixtures")
        .join(name)
}

/// The name `bad<invalid>name`, where `<invalid>` is a byte (Unix) or an unpaired surrogate (Windows) that no Unicode
/// text holds, so `to_string_lossy` turns it into U+FFFD. macOS file systems refuse such a name.
pub fn non_unicode_name() -> std::ffi::OsString {
    #[cfg(unix)]
    {
        std::os::unix::ffi::OsStringExt::from_vec(vec![b'b', b'a', b'd', 0xFF, b'n', b'a', b'm', b'e'])
    }
    #[cfg(windows)]
    {
        let wide: Vec<u16> = "bad"
            .encode_utf16()
            .chain([0xD800])
            .chain("name".encode_utf16())
            .collect();
        std::os::windows::ffi::OsStringExt::from_wide(&wide)
    }
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

fn chatty_stderr() -> crate::progress::LineProgress {
    crate::progress::LineProgress::stderr(crate::progress::Look {
        colour: false,
        verbose: true,
    })
}

/// A sink that prints everything to stderr, for a test that runs long enough for its progress to be read live.
pub fn stderr_progress() -> std::sync::Arc<crate::progress::LineProgress> {
    std::sync::Arc::new(chatty_stderr())
}

/// A progress sink the test reads back.
#[derive(Default)]
pub struct MemoryProgress {
    events: std::sync::Mutex<Vec<(std::time::SystemTime, crate::progress::Event)>>,
    echo: Option<crate::progress::LineProgress>,
}

impl MemoryProgress {
    /// A sink that also prints each note to stderr, for a test that runs long enough for its progress to be read live.
    pub fn echoing() -> MemoryProgress {
        MemoryProgress {
            echo: Some(chatty_stderr()),
            ..MemoryProgress::default()
        }
    }

    /// The text of every note and output line, at every level, in order.
    pub fn messages(&self) -> Vec<String> {
        self.messages_at().into_iter().map(|(_, text)| text).collect()
    }

    /// The notes and output lines, each with the time it arrived.
    pub fn messages_at(&self) -> Vec<(std::time::SystemTime, String)> {
        self.events_at()
            .into_iter()
            .filter_map(|(at, event)| match event {
                crate::progress::Event::Note { text, .. } | crate::progress::Event::Output { line: text } => {
                    Some((at, text))
                }
                _ => None,
            })
            .collect()
    }

    /// How many tasks began with a label that starts with `prefix`.
    pub fn tasks_begun(&self, prefix: &str) -> usize {
        self.events()
            .iter()
            .filter(|event| matches!(event, crate::progress::Event::Begin { label, .. } if label.starts_with(prefix)))
            .count()
    }

    /// The text of every task status, in order.
    pub fn statuses(&self) -> Vec<String> {
        self.statuses_at().into_iter().map(|(_, text)| text).collect()
    }

    /// The task statuses, each with the time it arrived.
    pub fn statuses_at(&self) -> Vec<(std::time::SystemTime, String)> {
        self.events_at()
            .into_iter()
            .filter_map(|(at, event)| match event {
                crate::progress::Event::Status { text, .. } => Some((at, text)),
                _ => None,
            })
            .collect()
    }

    pub fn events(&self) -> Vec<crate::progress::Event> {
        self.events_at().into_iter().map(|(_, event)| event).collect()
    }

    fn events_at(&self) -> Vec<(std::time::SystemTime, crate::progress::Event)> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl crate::progress::Progress for MemoryProgress {
    fn event(&self, event: &crate::progress::Event) {
        if let Some(echo) = &self.echo {
            echo.event(event);
        }
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((std::time::SystemTime::now(), event.clone()));
    }
}

/// A progress sink that sends the text of every note down a channel, so a test can wait for "waiting for ..." as an
/// event.
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

impl crate::progress::Progress for ChannelProgress {
    fn event(&self, event: &crate::progress::Event) {
        if let crate::progress::Event::Note { text, .. } = event {
            let _ = self.0.lock().unwrap().send(text.clone());
        }
    }
}
