//! What a running command tells the person: notes and waits.

use std::io::Write;

/// Receives human-readable progress and "waiting for ..." messages.
pub trait Progress: Send + Sync {
    fn message(&self, text: &str);
}

/// Prints each message on its own stderr line.
pub struct StderrProgress;

impl Progress for StderrProgress {
    fn message(&self, text: &str) {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{text}");
    }
}

/// Discards messages.
pub struct NullProgress;

impl Progress for NullProgress {
    fn message(&self, _text: &str) {}
}
