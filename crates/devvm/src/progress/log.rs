//! The log file: every event of one run, for reading afterwards.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

use cosca::identity::{Liveness, Resolved};

use crate::error::{Error, ErrorKind, Result};
use crate::host::{self, failed, is_absent, remove_if_present};
use crate::progress::{Event, Level, Outcome, Progress, TaskId, Total, escape_controls};
use crate::random;
use crate::store::layers::Clock;

mod tail;
#[cfg(test)]
pub(crate) use tail::log_tail;

#[cfg(test)]
#[path = "log_tests.rs"]
mod log_tests;

#[cfg(all(test, unix))]
#[path = "log_unix_tests.rs"]
mod log_unix_tests;

/// A log older than this is removed when a new one is made, unless its run is alive.
const KEEP: Duration = Duration::from_secs(14 * 24 * 3600);

/// Ends the name of a kept copy of a serial log.
const SERIAL_COPY: &str = ".serial.log";

/// Ends the name of a run's log.
const RUN_LOG: &str = ".log";

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
/// from the moment it exists, because a step's output can hold secrets.
pub struct FileLog {
    path: PathBuf,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    /// Appends [`FileLog::failed`]'s record, so that the ending never takes `state`.
    tail: File,
    /// The first error of a write to the log, from `state` or `tail`: the log is then missing records.
    first_error: OnceLock<String>,
}

struct State {
    out: Box<dyn Write + Send>,
    bars: HashMap<TaskId, Bar>,
    /// Set by [`FileLog::stop`]; checked under the lock by every event.
    stopped: bool,
}

struct Bar {
    total: Total,
    /// The 5% steps already logged.
    steps: u128,
}

impl FileLog {
    /// Makes `devvm-<UTC time>-<pid>-<4 hex>.log` in `dir`, which is made if it is not there, after removing the logs
    /// in it that are older than 14 days and whose process is not running. A log that cannot be removed is a `WARN`
    /// record of the new one. Fails when the directory or the file cannot be made.
    pub fn create(dir: &Path, clock: Arc<dyn Clock>) -> Result<FileLog> {
        FileLog::create_with(dir, clock, &process_is_alive)
    }

    /// [`FileLog::create`], with `alive` telling whether the process of a pid is running.
    pub(crate) fn create_with(dir: &Path, clock: Arc<dyn Clock>, alive: &dyn Fn(u32) -> bool) -> Result<FileLog> {
        std::fs::create_dir_all(dir).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot create the log directory {}", dir.display()),
                e,
            )
        })?;
        let warnings = prune(dir, clock.now(), alive);
        let header = format!(
            "{} INFO devvm {}\n",
            stamp(clock.now(), TIME),
            env!("CARGO_PKG_VERSION")
        );
        let (path, out) =
            create_named(dir, &*clock, RUN_LOG, header.as_bytes()).map_err(|(path, e)| cannot_write(&path, e))?;
        // One file, made private: a second open of the name could find another file under it.
        let tail = match out.try_clone() {
            Ok(tail) => tail,
            Err(e) => {
                drop(out);
                return Err(cannot_write(&path, e)
                    .after_cleanup(&format!("the log {}", path.display()), remove_if_present(&path)));
            }
        };
        let log = FileLog {
            path,
            clock,
            state: Mutex::new(State {
                out: Box::new(out),
                bars: HashMap::new(),
                stopped: false,
            }),
            tail,
            first_error: OnceLock::new(),
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
        match self.first_error.get() {
            Some(error) => format!("{path} (incomplete: {error})"),
            None => path.to_string(),
        }
    }

    /// Waits for the record being written, if any, then records no event more: the command line's ending stops the log
    /// before it calls [`FileLog::failed`].
    pub fn stop(&self) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).stopped = true;
    }

    /// Records `line`, the `devvm-error:` line that ends the run, as one `ERROR` record. It is not an event: no other
    /// sink sees it. It takes no lock, so it works from a panic hook on a thread that is inside the log. The ending
    /// calls it once the log is stopped; a hook does not stop it, and an event of another thread can follow the record.
    pub fn failed(&self, line: &str) {
        let record = self.record_line("ERROR", line);
        if let Err(e) = (&self.tail).write_all(record.as_bytes()) {
            let _ = self.first_error.set(e.to_string());
        }
    }

    fn record_line(&self, kind: &str, text: &str) -> String {
        format!("{} {kind} {}\n", stamp(self.clock.now(), TIME), escape_controls(text))
    }

    fn record(&self, state: &mut State, kind: &str, text: &str) {
        // The run goes on; `describe` tells at its end.
        if let Err(e) = state.out.write_all(self.record_line(kind, text).as_bytes()) {
            let _ = self.first_error.set(e.to_string());
        }
    }
}

/// Copies `source` to a new `devvm-<UTC time>-<pid>-<4 hex>.serial.log` in `dir` (made if missing), pruning old logs
/// first as [`FileLog::create`] does. Prune problems go to `warn`. The copy is the user's alone and streamed. An
/// unreadable `source` makes no file; a copy that fails half way is removed. Returns the copy's path.
#[allow(dead_code, reason = "no caller yet")]
pub(crate) fn keep_copy(
    dir: &Path,
    clock: &dyn Clock,
    source: &Path,
    warn: &mut dyn FnMut(&str),
) -> std::io::Result<PathBuf> {
    keep_copy_with(dir, clock, source, warn, &process_is_alive)
}

/// [`keep_copy`], with `alive` telling whether the process of a pid is running.
pub(crate) fn keep_copy_with(
    dir: &Path,
    clock: &dyn Clock,
    source: &Path,
    warn: &mut dyn FnMut(&str),
    alive: &dyn Fn(u32) -> bool,
) -> std::io::Result<PathBuf> {
    let mut from = File::open(source).map_err(|e| failed(format_args!("cannot read {}", source.display()), e))?;
    std::fs::create_dir_all(dir).map_err(|e| failed(format_args!("cannot create {}", dir.display()), e))?;
    for warning in prune(dir, clock.now(), alive) {
        warn(&warning);
    }
    let (path, mut to) = create_named(dir, clock, SERIAL_COPY, b"")
        .map_err(|(path, e)| failed(format_args!("cannot write {}", path.display()), e))?;
    // An error the file system reports only at flush is a failed copy too.
    let copied = std::io::copy(&mut from, &mut to).and_then(|_| to.sync_all());
    drop(to);
    if let Err(e) = copied {
        if let Err(removed) = remove_if_present(&path) {
            warn(&format!("cannot remove the partial copy {}: {removed}", path.display()));
        }
        return Err(failed(
            format_args!("cannot copy {} to {}", source.display(), path.display()),
            e,
        ));
    }
    Ok(path)
}

/// Makes a uniquely named file ending in `suffix` in `dir`, the user's alone, holding `first`; returns it open. The name
/// is redrawn while another run holds it. A failure carries the path it could not write.
fn create_named(
    dir: &Path,
    clock: &dyn Clock,
    suffix: &str,
    first: &[u8],
) -> std::result::Result<(PathBuf, File), (PathBuf, std::io::Error)> {
    loop {
        let name = format!(
            "devvm-{}-{}-{}{suffix}",
            stamp(clock.now(), "%Y%m%dT%H%M%SZ"),
            std::process::id(),
            hex::encode(random::bytes::<2>())
        );
        let path = dir.join(name);
        match host::current().write_private(&path, first) {
            Ok(file) => return Ok((path, file)),
            // Another run drew the same name: draw again.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err((path, e)),
        }
    }
}

fn cannot_write(path: &Path, e: std::io::Error) -> Error {
    Error::io(ErrorKind::Io, format!("cannot write the log {}", path.display()), e)
}

/// `time` in UTC as `format` says.
fn stamp(time: SystemTime, format: &str) -> String {
    jiff::Timestamp::try_from(time).map_or_else(|_| "out-of-range".to_owned(), |at| at.strftime(format).to_string())
}

/// Whether the process `pid` is running. A pid the OS will not say anything about is taken as running, and so is a
/// reused one: a log is then kept too long, never removed under its run.
fn process_is_alive(pid: u32) -> bool {
    match cosca::identity::ProcessId::of(pid) {
        Resolved::Gone => false,
        Resolved::Unknown => true,
        Resolved::Found(id) => id.is_alive() != Liveness::Dead,
    }
}

/// The pid in the name of a log `devvm-<time>-<pid>-<hex>` plus its suffix.
fn owner_of(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("devvm-")?.strip_suffix(RUN_LOG)?;
    rest.rsplit('-').nth(1)?.parse().ok()
}

/// Removes the logs in `dir` that are older than [`KEEP`] by `now`, except the run logs of a process that `alive` says is
/// running; returns what went wrong. Works from one listing of the directory.
fn prune(dir: &Path, now: SystemTime, alive: &dyn Fn(u32) -> bool) -> Vec<String> {
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
        if !name.starts_with("devvm-") || !name.ends_with(RUN_LOG) {
            continue;
        }
        let modified = match entry.metadata() {
            Ok(meta) if meta.is_file() => meta.modified(),
            Ok(_) => continue,
            Err(e) => Err(e),
        };
        prune_one(&entry.path(), name, modified, now, alive, &mut warnings);
    }
    warnings
}

/// Removes the log at `path` if it is older than [`KEEP`] and not a live run's; adds problems to `warnings`.
fn prune_one(
    path: &Path,
    name: &str,
    modified: std::io::Result<SystemTime>,
    now: SystemTime,
    alive: &dyn Fn(u32) -> bool,
    warnings: &mut Vec<String>,
) {
    match modified {
        Ok(modified) if now.duration_since(modified).is_ok_and(|age| age > KEEP) => {
            // A kept copy is complete when made; a run log may still be written.
            if !name.ends_with(SERIAL_COPY) && owner_of(name).is_some_and(alive) {
                return;
            }
            if let Err(e) = remove_if_present(path) {
                warnings.push(format!("cannot remove the old log {}: {e}", path.display()));
            }
        }
        Ok(_) => {}
        // Another prune removed it between the listing and the look.
        Err(e) if is_absent(&e) => {}
        Err(e) => warnings.push(format!("cannot tell the age of the log {}: {e}", path.display())),
    }
}

impl Progress for FileLog {
    fn event(&self, event: &Event) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.stopped {
            return;
        }
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
