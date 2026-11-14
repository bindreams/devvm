//! How the VM suites wait on a guest: a failure bound that names what did not happen, and a log of each step. `vm_cli`
//! includes this file by `#[path]`, so it needs nothing from the lib.
//!
//! What a test awaits in the guest (a command's answer, a flag file, a process that ends, a boot) may never happen. Each
//! bound is the failure bound surfaced to the person watching the run: its expiry ends the run with a message that names
//! what did not happen, and is never taken as a fact about the guest. Each is sized well above the longest measured case
//! of its kind.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

/// A command's answer from the guest, and what a host process (`ssh`, `ssh-keyscan`, `devvm`) takes to end.
pub const ANSWER_BOUND: Duration = Duration::from_secs(10 * 60);

/// Prints a step of the test that runs it, so that a log which stops shows where the test was.
pub fn step(what: &str) {
    eprintln!("{}: {what}", std::thread::current().name().unwrap_or("a test"));
}

/// Ends the process unless it is dropped within the bound it was made with.
#[must_use = "a watchdog that is dropped at once guards nothing"]
pub struct Watchdog {
    _done: mpsc::Sender<()>,
}

/// Logs that the test awaits `what`, and guards the wait: a call that blocks in the guest and cannot be given up on from
/// outside (a command's answer, a host process's end). When `bound` passes before the guard is dropped, it prints that
/// `what` did not finish and ends the whole test process: a thread that waits on the guest cannot be told to stop, so the
/// job goes on to its next step and to the serial-log upload instead of ending at its own limit.
pub fn awaiting(what: impl Into<String>, bound: Duration) -> Watchdog {
    let what = what.into();
    step(&format!("awaiting {what}"));
    let test = std::thread::current().name().unwrap_or("a test").to_owned();
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        if finished.recv_timeout(bound) == Err(RecvTimeoutError::Timeout) {
            eprintln!("{test}: {what} did not finish within {bound:?}; ending the test process");
            std::process::exit(1);
        }
    });
    Watchdog { _done: done }
}

/// [`awaiting`] the guest's answer to `what`, within [`ANSWER_BOUND`]. `what` names the step: a command's argv when that
/// is short and says what runs ([`named`]), the caller's own label where the argv is a PowerShell wrapper.
pub fn answer_to(what: &str) -> Watchdog {
    awaiting(format!("the guest's answer to {what}"), ANSWER_BOUND)
}

/// `argv` as a message names it: the whole line, cut at 160 characters.
pub fn named(argv: &[String]) -> String {
    cut(&format!("{argv:?}"))
}

/// `line` cut at 160 characters.
pub fn cut(line: &str) -> String {
    match line.char_indices().nth(160) {
        Some((end, _)) => format!("{}...", &line[..end]),
        None => line.to_owned(),
    }
}
