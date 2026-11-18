//! The log file: every event of one run, for reading afterwards.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::error::{Error, ErrorKind, Result};
use crate::host::{self, Hold, HoldOutcome, Host, claim_of_new_name, remove_if_present, symlink_metadata_if_present};
use crate::progress::owned::Owned;
use crate::progress::{Event, Level, Outcome, Progress, TaskId, Total, escape_controls};
use crate::random;
use crate::store::layers::Clock;

#[cfg(test)]
#[path = "log_tests.rs"]
mod log_tests;

/// A log older than this is removed when a new one is made, unless its run is alive.
const KEEP: Duration = Duration::from_secs(14 * 24 * 3600);

const ADVANCES: u128 = 20;

const TIME: &str = "%Y-%m-%dT%H:%M:%S%.3fZ";

/// Every event of one run: `<UTC timestamp to ms> <KIND> <text>`, one record per line (controls in the text are
/// escaped).
///
/// Kinds: `DEBUG`, `INFO` and `WARN` for notes, `OUTPUT` for a step's output line, `ERROR` for [`FileLog::failed`], and
/// `TASK <id> <what>` for a task: `begin parent=<id|-> <unknown|bytes=N|items=N> <label>`, `status <text>`,
/// `advance <position>` and `end <finished|abandoned>`. A task counted in bytes logs an advance each 5% only; the
/// other tasks log each advance they report. A `begin` names its parent even where this log never saw it begin.
///
/// Each record is written when the event arrives, so a killed process leaves its log. The file is the user's alone
/// from the moment it exists, because a step's output can hold secrets. The run holds `<log>.lock` while it lives, so
/// that no other run removes its log.
pub struct FileLog {
    path: PathBuf,
    clock: Arc<dyn Clock>,
    /// A record that a thread attempts while it holds the state is skipped: waiting would wait for itself. A panic
    /// inside the log reaches the panic hook on that thread, so the hook's own record is lost.
    state: Owned<State>,
}

struct State {
    out: Box<dyn Write + Send>,
    bars: HashMap<TaskId, Bar>,
    /// The first error of a write to `out`.
    failed: Option<String>,
    /// The run's hold on the lock file; ends with the process.
    _hold: Box<dyn Hold>,
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
        let host = host::current();
        let warnings = prune(host, dir, clock.now());
        let (path, hold) = loop {
            let name = format!(
                "devvm-{}-{}-{}.log",
                stamp(clock.now(), "%Y%m%dT%H%M%SZ"),
                std::process::id(),
                hex::encode(random::bytes::<2>())
            );
            let path = dir.join(name);
            let lock = lock_path(&path);
            // The lock comes first: a log that exists always has a hold that tells its run is alive.
            let hold = match host.hold_new(&lock) {
                Ok(hold) => hold,
                // Another run drew the same name: draw again.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(cannot_write(&lock, e)),
            };
            let header = format!(
                "{} INFO devvm {}\n",
                stamp(clock.now(), TIME),
                env!("CARGO_PKG_VERSION")
            );
            match host.write_private(&path, header.as_bytes()) {
                Ok(()) => break (path, hold),
                // A log of that name is there without a lock: draw again. A lock that stays is pruned later.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let _ = hold.delete();
                }
                Err(e) => {
                    let _ = hold.delete();
                    return Err(cannot_write(&path, e));
                }
            }
        };
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| cannot_write(&path, e))?;
        let log = FileLog {
            path,
            clock,
            state: Owned::new(State {
                out: Box::new(file),
                bars: HashMap::new(),
                failed: None,
                _hold: hold,
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

    /// The path, and the first error of a write that failed: the log is then missing records.
    pub fn describe(&self) -> String {
        let path = self.path.display();
        match self.state.lock().and_then(|state| state.failed.clone()) {
            Some(error) => format!("{path} (incomplete: {error})"),
            None => path.to_string(),
        }
    }

    /// Records `line`, the `devvm-error:` line that ends the run, as one `ERROR` record. It is not an event: no other
    /// sink sees it.
    pub fn failed(&self, line: &str) {
        let Some(mut state) = self.state.lock() else { return };
        self.record(&mut state, "ERROR", line);
    }

    fn record(&self, state: &mut State, kind: &str, text: &str) {
        let line = format!("{} {kind} {}\n", stamp(self.clock.now(), TIME), escape_controls(text));
        // The run goes on; `describe` tells at its end.
        if let Err(e) = state.out.write_all(line.as_bytes()) {
            state.failed.get_or_insert_with(|| e.to_string());
        }
    }
}

const LOCK_SUFFIX: &str = ".lock";

/// The lock file of the log at `log`.
pub(crate) fn lock_path(log: &Path) -> PathBuf {
    let mut name = log.as_os_str().to_owned();
    name.push(LOCK_SUFFIX);
    PathBuf::from(name)
}

fn cannot_write(path: &Path, e: std::io::Error) -> Error {
    Error::io(ErrorKind::Io, format!("cannot write the log {}", path.display()), e)
}

/// `time` in UTC as `format` says.
fn stamp(time: SystemTime, format: &str) -> String {
    jiff::Timestamp::try_from(time).map_or_else(|_| "out-of-range".to_owned(), |at| at.strftime(format).to_string())
}

/// Removes the logs in `dir` that are older than [`KEEP`] by `now`, except those of a run that is alive, and the lock
/// files whose log is gone; returns what went wrong.
fn prune(host: &dyn Host, dir: &Path, now: SystemTime) -> Vec<String> {
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
        if !name.starts_with("devvm-") {
            continue;
        }
        if name.ends_with(".log") {
            let modified = match entry.metadata() {
                Ok(meta) if meta.is_file() => meta.modified(),
                Ok(_) => continue,
                Err(e) => Err(e),
            };
            match modified {
                Ok(modified) if now.duration_since(modified).is_ok_and(|age| age > KEEP) => {
                    remove_unless_alive(host, &entry.path(), &mut warnings);
                }
                Ok(_) => {}
                Err(e) => warnings.push(format!(
                    "cannot tell the age of the log {}: {e}",
                    entry.path().display()
                )),
            }
        } else if let Some(log) = log_of_lock_name(name)
            && matches!(symlink_metadata_if_present(&dir.join(log)), Ok(None))
        {
            remove_lock_unless_held(host, &entry.path(), &mut warnings);
        }
    }
    warnings
}

/// The log `name` is the lock file of, while it is made too (`<log>.lock.new`): the inverse of [`lock_path`].
fn log_of_lock_name(name: &str) -> Option<&str> {
    let lock = claim_of_new_name(name).unwrap_or(name);
    lock.strip_suffix(LOCK_SUFFIX).filter(|log| log.ends_with(".log"))
}

/// Removes the log at `path` and its lock, unless the lock is held: its run is alive.
fn remove_unless_alive(host: &dyn Host, path: &Path, warnings: &mut Vec<String>) {
    let lock = lock_path(path);
    let hold = match host.hold_existing(&lock) {
        Ok(HoldOutcome::Busy) => return,
        Ok(HoldOutcome::Taken(hold)) => Some(hold),
        Ok(HoldOutcome::Absent) => None,
        Err(e) => {
            warnings.push(format!("cannot tell whether the log {} is in use: {e}", path.display()));
            return;
        }
    };
    if let Err(e) = remove_if_present(path) {
        warnings.push(format!("cannot remove the old log {}: {e}", path.display()));
    } else if let Some(hold) = hold
        && let Err(e) = hold.delete()
    {
        warnings.push(format!("cannot remove the lock {}: {e}", lock.display()));
    }
}

/// Removes the lock file at `path`, whose log is gone, unless a run holds it: it is making its log.
fn remove_lock_unless_held(host: &dyn Host, path: &Path, warnings: &mut Vec<String>) {
    match host.hold_existing(path) {
        Ok(HoldOutcome::Taken(hold)) => {
            if let Err(e) = hold.delete() {
                warnings.push(format!("cannot remove the lock {}: {e}", path.display()));
            }
        }
        Ok(HoldOutcome::Busy | HoldOutcome::Absent) => {}
        Err(e) => warnings.push(format!(
            "cannot tell whether the lock {} is in use: {e}",
            path.display()
        )),
    }
}

impl Progress for FileLog {
    fn event(&self, event: &Event) {
        let Some(mut state) = self.state.lock() else { return };
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
