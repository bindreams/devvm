//! The library half of the exit and output contract: exit-code mapping, the status file, the one-line error.

use std::borrow::Cow;
use std::io::Write;
use std::path::Path;

use serde::Serialize;

use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "exit_tests.rs"]
mod exit_tests;

/// The exit code of devvm's own failures.
pub const DEVVM_ERROR_EXIT: i32 = 125;

// Guest results =======================================================================================================

/// How a guest command ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    Code(i32),
    Signal { name: String, core_dumped: bool },
}

/// A finished guest command.
#[derive(Clone, Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn stdout_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    pub fn stderr_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }
}

/// How many bits of a guest exit code the host can pass on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitWidth {
    /// The full 32-bit code (Windows hosts).
    Full32,
    /// The low byte only (Unix hosts).
    LowByte,
}

impl ExitWidth {
    /// The host process exit code for a guest result.
    ///
    /// A non-zero code is never reported as success or as devvm's own failure: under `LowByte`, a non-zero code whose
    /// low byte is 0 or 125 becomes 1. A signal is `128 + number`; a signal name SSH does not define becomes 1, and
    /// the status file carries the true name.
    pub fn exit_code(self, status: &ExitStatus) -> i32 {
        match status {
            ExitStatus::Code(code) => match self {
                ExitWidth::Full32 => *code,
                ExitWidth::LowByte => match *code & 0xff {
                    _ if *code == 0 => 0,
                    0 | 125 => 1,
                    low => low,
                },
            },
            ExitStatus::Signal { name, .. } => signal_number(name).map_or(1, |number| 128 + number),
        }
    }
}

/// Linux signal numbers for the names SSH defines (RFC 4254 section 6.10), plus BUS and TRAP.
fn signal_number(name: &str) -> Option<i32> {
    Some(match name {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "ILL" => 4,
        "TRAP" => 5,
        "ABRT" => 6,
        "BUS" => 7,
        "FPE" => 8,
        "KILL" => 9,
        "USR1" => 10,
        "SEGV" => 11,
        "USR2" => 12,
        "PIPE" => 13,
        "ALRM" => 14,
        "TERM" => 15,
        _ => return None,
    })
}

// Reports =============================================================================================================

/// What the status file says.
pub enum Report<'a> {
    Guest(&'a ExitStatus),
    Devvm(&'a Error),
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum Wire<'a> {
    Guest(WireGuest<'a>),
    Devvm(WireDevvm<'a>),
}

#[derive(Serialize)]
#[serde(untagged)]
enum WireGuest<'a> {
    Code { code: i32 },
    Signal { signal: &'a str, core_dumped: bool },
}

#[derive(Serialize)]
struct WireDevvm<'a> {
    kind: &'static str,
    message: &'a str,
}

/// The status file's JSON text (no trailing newline).
pub fn status_json(report: &Report<'_>) -> String {
    let wire = match report {
        Report::Guest(ExitStatus::Code(code)) => Wire::Guest(WireGuest::Code { code: *code }),
        Report::Guest(ExitStatus::Signal { name, core_dumped }) => Wire::Guest(WireGuest::Signal {
            signal: name,
            core_dumped: *core_dumped,
        }),
        Report::Devvm(error) => Wire::Devvm(WireDevvm {
            kind: error.kind().as_str(),
            message: error.message(),
        }),
    };
    serde_json::to_string(&wire).expect("a status report always serializes")
}

/// Writes the status file atomically: a temp file in the same directory, renamed over `path`.
pub fn write_status_file(path: &Path, report: &Report<'_>) -> Result<()> {
    let fail = |source: std::io::Error| {
        Error::io(
            ErrorKind::StatusFile,
            format!("cannot write {}", path.display()),
            source,
        )
    };
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(fail)?;
    file.write_all(status_json(report).as_bytes()).map_err(fail)?;
    file.as_file().sync_all().map_err(fail)?;
    // `std::fs::rename`, not `persist`: std replaces atomically even while a reader has `path` open on Windows.
    let mut temp = file.into_temp_path();
    std::fs::rename(&temp, path).map_err(fail)?;
    temp.disable_cleanup(true);
    Ok(())
}

/// `devvm-error: <kind>: <message>` on one line: control characters are escaped, so the line has no newline inside.
pub fn format_error_line(error: &Error) -> String {
    let mut line = format!("devvm-error: {}: ", error.kind().as_str());
    for c in error.message().chars() {
        if c.is_control() {
            line.extend(c.escape_default());
        } else {
            line.push(c);
        }
    }
    line
}

/// Ends a command: writes the status file (when asked), prints devvm's own error line, and returns the process exit
/// code. A status file that cannot be written is itself a devvm failure (125).
pub fn finish(
    status_file: Option<&Path>,
    outcome: Result<ExitStatus>,
    width: ExitWidth,
    stderr: &mut dyn Write,
) -> i32 {
    let (report, exit) = match &outcome {
        Ok(status) => (Report::Guest(status), width.exit_code(status)),
        Err(error) => (Report::Devvm(error), DEVVM_ERROR_EXIT),
    };
    let written = match status_file {
        Some(path) => write_status_file(path, &report),
        None => Ok(()),
    };
    // One line either way: when the status file fails, its line carries the original error too.
    let (line, exit) = match (written, &outcome) {
        (Ok(()), Ok(_)) => return exit,
        (Ok(()), Err(error)) => (format_error_line(error), exit),
        (Err(failure), Ok(_)) => (format_error_line(&failure), DEVVM_ERROR_EXIT),
        (Err(failure), Err(original)) => {
            let message = format!(
                "{}; original: {}: {}",
                failure.message(),
                original.kind().as_str(),
                original.message()
            );
            (
                format_error_line(&Error::new(ErrorKind::StatusFile, message)),
                DEVVM_ERROR_EXIT,
            )
        }
    };
    let _ = writeln!(stderr, "{line}");
    exit
}

// Progress ============================================================================================================

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
