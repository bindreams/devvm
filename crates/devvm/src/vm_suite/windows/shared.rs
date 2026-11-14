//! What the lib's Windows suite and `devvm-cli`'s `vm_cli` do the same way when they build a Windows base. `vm_cli`
//! includes this file by `#[path]`, so it needs nothing from the lib.

use std::path::Path;
use std::sync::OnceLock;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

/// Runs `make` unless an earlier call to it, with the same `slot`, failed: skuld runs a fixture's setup again after it
/// fails, and each attempt is a full Windows install or boot, so the first failure stands for every later test.
pub fn first_try<T>(slot: &OnceLock<String>, make: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    if let Some(error) = slot.get() {
        return Err(format!("the first attempt of this process failed: {error}"));
    }
    make().inspect_err(|error| {
        let _ = slot.set(error.clone());
    })
}

/// The text of a recipe that installs Windows from `iso` and runs `steps`, each one step's YAML.
pub fn recipe_text(iso: &Path, steps: &[&str]) -> Result<String, String> {
    let iso = iso.to_str().ok_or("a UTF-8 ISO path")?;
    let iso = serde_json::to_string(iso).map_err(|e| e.to_string())?;
    let steps = if steps.is_empty() {
        "steps: []\n".to_owned()
    } else {
        format!(
            "steps:\n{}",
            steps.iter().map(|step| format!("  - {step}\n")).collect::<String>()
        )
    };
    Ok(format!("from: {{ iso: {iso} }}\n{steps}"))
}

// Waits on the guest ==================================================================================================
//
// What a test awaits in the guest (a command's answer, a flag file, a process that ends, a boot) may never happen. Each
// bound below is the failure bound surfaced to the person watching the run: its expiry fails the test with a message
// that names what did not happen, and is never taken as a fact about the guest. Each is sized well above the longest
// measured case of its kind.

/// A command's answer from the guest, and what a host process (`ssh`, `ssh-keyscan`, `devvm`) takes to end.
pub const ANSWER_BOUND: Duration = Duration::from_secs(10 * 60);

/// A condition in the guest that a wait re-checks: a flag file, a process that ends, a request that is removed. It is
/// shorter than [`ANSWER_BOUND`], so a guest-side wait that fails reports itself before the command that runs it is given
/// up on.
pub const STATE_BOUND: Duration = Duration::from_secs(5 * 60);

/// The next output of a terminal session.
pub const TERMINAL_BOUND: Duration = Duration::from_secs(5 * 60);

/// A restart of an instance: twice the engine's own bound on a booting guest.
pub const BOOT_BOUND: Duration = Duration::from_secs(20 * 60);

/// Prints a step of the test that runs it, so that a log which stops shows where the test was.
pub fn step(what: &str) {
    eprintln!("{}: {what}", std::thread::current().name().unwrap_or("a test"));
}

/// Ends the process unless it is dropped within the bound it was made with.
#[must_use = "a watchdog that is dropped at once guards nothing"]
pub struct Watchdog {
    _done: mpsc::Sender<()>,
}

/// Guards a call that blocks in the guest and cannot be given up on from outside (a command's answer, a host process's
/// end). When `bound` passes before the guard is dropped, it prints that `what` did not finish and ends the whole test
/// process: a thread that waits on the guest cannot be told to stop, so the job goes on to its next step and to the
/// serial-log upload instead of ending at its own limit.
pub fn watch(what: impl Into<String>, bound: Duration) -> Watchdog {
    let what = what.into();
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

/// `argv` as a message names it: the whole line, cut at 160 characters.
pub fn named(argv: &[String]) -> String {
    let line = format!("{argv:?}");
    match line.char_indices().nth(160) {
        Some((end, _)) => format!("{}...", &line[..end]),
        None => line,
    }
}
