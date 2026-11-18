//! The log file: every event of one run, for reading afterwards.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use crate::error::{Error, ErrorKind, Result};
use crate::exit::format_error_line;
use crate::host::{self, remove_if_present};
use crate::progress::{Event, Level, Outcome, Progress, TaskId, Total, escape_controls};
use crate::random;
use crate::store::layers::Clock;

#[cfg(test)]
#[path = "log_tests.rs"]
mod log_tests;

/// A log older than this is removed when a new one is made.
const KEEP: Duration = Duration::from_secs(14 * 24 * 3600);

/// How many advances a task counted in bytes logs: one per 5%.
const ADVANCES: u128 = 20;

const TIME: &str = "%Y-%m-%dT%H:%M:%S%.3fZ";

/// Every event of one run, one record per line: `<UTC timestamp to ms> <KIND> <text>`, with each control character in
/// the text written as an escape so a record is one line.
///
/// Kinds: `DEBUG`, `INFO` and `WARN` for notes, `OUTPUT` for a step's output line, `ERROR` for [`FileLog::failed`], and
/// `TASK <id> <what>` for a task: `begin parent=<id|-> <unknown|bytes=N|items=N> <label>`, `status <text>`,
/// `advance <position>` and `end <finished|abandoned>`. A task counted in bytes logs an advance each 5% only; the
/// other tasks log each advance they report. A `begin` names its parent even where this log never saw it begin.
///
/// Each record is written when the event arrives, so a killed process leaves its log. The file is the user's alone
/// from the moment it exists, because a step's output can hold secrets.
pub struct FileLog {
    path: PathBuf,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

struct State {
    file: File,
    bars: HashMap<TaskId, Bar>,
}

struct Bar {
    total: Total,
    /// The 5% steps already logged.
    steps: u128,
}

impl FileLog {
    /// Makes `devvm-<UTC time>-<pid>-<4 hex>.log` in `dir`, which is made if it is not there, after removing the logs
    /// in it that are older than 14 days. A log that cannot be removed is a `WARN` record of the new one. Fails when
    /// the directory or the file cannot be made.
    pub fn create(dir: &Path, clock: Arc<dyn Clock>) -> Result<FileLog> {
        std::fs::create_dir_all(dir).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot create the log directory {}", dir.display()),
                e,
            )
        })?;
        let warnings = prune(dir, clock.now());
        let host = host::current();
        let path = loop {
            let name = format!(
                "devvm-{}-{}-{}.log",
                stamp(clock.now(), "%Y%m%dT%H%M%SZ"),
                std::process::id(),
                hex::encode(random::bytes::<2>())
            );
            let path = dir.join(name);
            let header = format!(
                "{} INFO devvm {}\n",
                stamp(clock.now(), TIME),
                env!("CARGO_PKG_VERSION")
            );
            match host.write_private(&path, header.as_bytes()) {
                Ok(()) => break path,
                // Another run drew the same name: draw again.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(cannot_write(&path, e)),
            }
        };
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| cannot_write(&path, e))?;
        let log = FileLog {
            path,
            clock,
            state: Mutex::new(State {
                file,
                bars: HashMap::new(),
            }),
        };
        for warning in warnings {
            log.warn(&warning);
        }
        Ok(log)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records `error`, the failure that ends the run, as one `ERROR` record with the text of the `devvm-error:` line.
    /// It is not an event: no other sink sees it.
    pub fn failed(&self, error: &Error) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.record(&mut state, "ERROR", &format_error_line(error));
    }

    fn record(&self, state: &mut State, kind: &str, text: &str) {
        let line = format!("{} {kind} {}\n", stamp(self.clock.now(), TIME), escape_controls(text));
        // A log that cannot be written has nobody to tell: the screen has its own copy.
        let _ = state.file.write_all(line.as_bytes());
    }
}

fn cannot_write(path: &Path, e: std::io::Error) -> Error {
    Error::io(ErrorKind::Io, format!("cannot write the log {}", path.display()), e)
}

/// `time` in UTC as `format` says.
fn stamp(time: SystemTime, format: &str) -> String {
    jiff::Timestamp::try_from(time).map_or_else(|_| "out-of-range".to_owned(), |at| at.strftime(format).to_string())
}

/// Removes the logs in `dir` that are older than [`KEEP`] by `now`; returns what went wrong.
fn prune(dir: &Path, now: SystemTime) -> Vec<String> {
    let mut warnings = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => return vec![format!("cannot list the logs in {}: {e}", dir.display())],
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                warnings.push(format!("cannot list the logs in {}: {e}", dir.display()));
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.starts_with("devvm-") && name.ends_with(".log")) {
            continue;
        }
        let modified = match entry.metadata() {
            Ok(meta) if meta.is_file() => meta.modified(),
            Ok(_) => continue,
            Err(e) => Err(e),
        };
        match modified {
            Ok(modified) if now.duration_since(modified).is_ok_and(|age| age > KEEP) => {
                if let Err(e) = remove_if_present(&entry.path()) {
                    warnings.push(format!("cannot remove the old log {}: {e}", entry.path().display()));
                }
            }
            Ok(_) => {}
            Err(e) => warnings.push(format!(
                "cannot tell the age of the log {}: {e}",
                entry.path().display()
            )),
        }
    }
    warnings
}

impl Progress for FileLog {
    fn event(&self, event: &Event) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let state = &mut *state;
        match event {
            Event::Note { level, text } => {
                let kind = match level {
                    Level::Debug => "DEBUG",
                    Level::Info => "INFO",
                    Level::Warn => "WARN",
                };
                self.record(state, kind, text);
            }
            Event::Output { line } => self.record(state, "OUTPUT", line),
            Event::Begin {
                id,
                parent,
                label,
                total,
            } => {
                let parent = parent.map_or_else(|| "-".to_owned(), |parent| parent.to_string());
                let total_text = match total {
                    Total::Unknown => "unknown".to_owned(),
                    Total::Bytes(bytes) => format!("bytes={bytes}"),
                    Total::Items(items) => format!("items={items}"),
                };
                state.bars.insert(
                    *id,
                    Bar {
                        total: *total,
                        steps: 0,
                    },
                );
                self.record(
                    state,
                    &format!("TASK {id}"),
                    &format!("begin parent={parent} {total_text} {label}"),
                );
            }
            Event::Status { id, text } => self.record(state, &format!("TASK {id}"), &format!("status {text}")),
            Event::Advance { id, position } => {
                let due = match state.bars.get_mut(id) {
                    Some(Bar {
                        total: Total::Bytes(total @ 1..),
                        steps,
                    }) => {
                        let reached = (u128::from(*position) * ADVANCES / u128::from(*total)).min(ADVANCES);
                        let due = reached > *steps;
                        *steps = (*steps).max(reached);
                        due
                    }
                    _ => true,
                };
                if due {
                    self.record(state, &format!("TASK {id}"), &format!("advance {position}"));
                }
            }
            Event::End { id, outcome } => {
                state.bars.remove(id);
                let outcome = match outcome {
                    Outcome::Finished => "finished",
                    Outcome::Abandoned => "abandoned",
                };
                self.record(state, &format!("TASK {id}"), &format!("end {outcome}"));
            }
        }
    }
}
