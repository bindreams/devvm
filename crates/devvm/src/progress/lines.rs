//! Concise lines: the screen for a stream that is not a terminal, and the default for library users.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;

use crate::progress::{
    Colour, Event, Level, Outcome, Progress, TaskId, Total, escape_controls, human_bytes, paint, strip_ansi,
};

#[cfg(test)]
#[path = "lines_tests.rs"]
mod lines_tests;

/// How [`LineProgress`] writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Look {
    /// Wraps `warning:` in yellow and `done` in green.
    pub colour: bool,
    /// Also writes [`Level::Debug`] notes and step output.
    pub verbose: bool,
}

/// The variable that asks for step output and debug notes.
pub const VERBOSE_VAR: &str = "DEVVM_VERBOSE";

/// Whether [`VERBOSE_VAR`] asks for them: set, and not empty, `0`, `false`, `no` or `off` in any case. The one reader
/// of the variable; the command line takes it from here too.
pub fn verbose_from_env() -> bool {
    verbose_in(&|name| std::env::var_os(name))
}

fn verbose_in(lookup: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    lookup(VERBOSE_VAR).is_some_and(|value| {
        let value = value.to_string_lossy().to_lowercase();
        !matches!(value.as_str(), "" | "0" | "false" | "no" | "off")
    })
}

impl Look {
    /// No colour, and verbose when [`verbose_from_env`] says so: how a library user's progress looks.
    pub fn from_env() -> Look {
        Look {
            colour: false,
            verbose: verbose_from_env(),
        }
    }
}

/// Writes one concise line per event worth a person's attention.
///
/// A sized task prints its size, then each quarter it passes, then `done`; an item count prints no quarters. Whole lines go out under one lock, so threads
/// never interleave inside a line. Controls in any text are escaped, so a record is one line.
pub struct LineProgress {
    state: Mutex<State>,
    look: Look,
}

struct State {
    out: Box<dyn Write + Send>,
    tasks: HashMap<TaskId, Open>,
}

struct Open {
    label: String,
    depth: usize,
    total: Total,
    /// The highest quarter (25, 50 or 75) already printed.
    quarter: u64,
}

impl LineProgress {
    pub fn stderr(look: Look) -> LineProgress {
        LineProgress::to(Box::new(std::io::stderr()), look)
    }

    pub fn to(out: Box<dyn Write + Send>, look: Look) -> LineProgress {
        LineProgress {
            state: Mutex::new(State {
                out,
                tasks: HashMap::new(),
            }),
            look,
        }
    }
}

impl State {
    fn line(&mut self, depth: usize, text: &str) {
        let mut line = " ".repeat(2 * depth);
        line.push_str(text);
        line.push('\n');
        // A closed stderr has nobody to tell.
        let _ = self.out.write_all(line.as_bytes());
    }
}

impl Progress for LineProgress {
    fn event(&self, event: &Event) {
        let look = self.look;
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match event {
            Event::Note { level, text } => {
                let text = escape_controls(text);
                match level {
                    Level::Debug if !look.verbose => {}
                    Level::Warn => {
                        let prefix = paint(look.colour, Colour::Yellow, "warning:");
                        state.line(0, &format!("{prefix} {text}"));
                    }
                    Level::Debug | Level::Info => state.line(0, &text),
                }
            }
            Event::Output { line } => {
                if look.verbose {
                    state.line(0, &escape_controls(&strip_ansi(line)));
                }
            }
            Event::Begin {
                id,
                parent,
                label,
                total,
            } => {
                let depth = parent
                    .and_then(|parent| state.tasks.get(&parent))
                    .map_or(0, |parent| parent.depth + 1);
                let label = escape_controls(label).into_owned();
                match total {
                    Total::Bytes(bytes) => state.line(depth, &format!("{label} ({})", human_bytes(*bytes))),
                    Total::Items(_) | Total::Unknown => state.line(depth, &label),
                }
                state.tasks.insert(
                    *id,
                    Open {
                        label,
                        depth,
                        total: *total,
                        quarter: 0,
                    },
                );
            }
            Event::Status { id, text } => {
                if let Some(open) = state.tasks.get(id) {
                    let line = format!("{}: {}", open.label, escape_controls(text));
                    let depth = open.depth;
                    state.line(depth, &line);
                }
            }
            Event::Advance { id, position } => {
                let Some(open) = state.tasks.get_mut(id) else { return };
                let Total::Bytes(total @ 1..) = open.total else { return };
                let percent = u128::from(*position) * 100 / u128::from(total);
                // 100% is the end of the task, which says `done`.
                let reached = [75, 50, 25]
                    .into_iter()
                    .find(|quarter| percent < 100 && percent >= u128::from(*quarter));
                if let Some(quarter) = reached.filter(|quarter| *quarter > open.quarter) {
                    open.quarter = quarter;
                    let (depth, line) = (open.depth, format!("{}: {quarter}%", open.label));
                    state.line(depth, &line);
                }
            }
            Event::End { id, outcome } => {
                let Some(open) = state.tasks.remove(id) else { return };
                if *outcome == Outcome::Finished && !matches!(open.total, Total::Unknown) {
                    let done = paint(look.colour, Colour::Green, "done");
                    state.line(open.depth, &format!("{}: {done}", open.label));
                }
            }
        }
    }
}
