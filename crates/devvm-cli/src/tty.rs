//! Progress bars and a task tree on a terminal.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use devvm::progress::{Colour, Event, Level, Look, Progress, TaskId, Total, escape_controls, human_bytes, paint};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};

#[cfg(test)]
#[path = "tty_tests.rs"]
mod tty_tests;

/// How often a terminal is redrawn.
const REFRESH_HZ: u8 = 15;

/// Below this many columns a sized task shows no bar: its line would wrap.
const BAR_COLUMNS: u16 = 50;

/// Draws one line per open task, children under their parent with `├─` and `└─`, and prints notes above them.
///
/// A task that ends leaves no line. A sized task draws a bar and its position in the units of
/// [`devvm::progress::human_bytes`]; a task of unknown size is a spinner. A `Begin` whose parent this screen never
/// saw is drawn as a root.
pub(crate) struct TtyProgress {
    multi: MultiProgress,
    look: Look,
    tick: Option<Duration>,
    columns: u16,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The open tasks in the order they are drawn: a parent, then each of its children with their own.
    order: Vec<TaskId>,
    tasks: HashMap<TaskId, Slot>,
}

struct Slot {
    bar: ProgressBar,
    label: String,
    status: Option<String>,
    total: Total,
    depth: usize,
}

impl TtyProgress {
    /// Draws to `target`, which is `columns` wide; spinners tick by themselves each `tick`, or only when their task
    /// reports, with `None`.
    pub(crate) fn new(target: ProgressDrawTarget, look: Look, tick: Option<Duration>, columns: u16) -> TtyProgress {
        TtyProgress {
            multi: MultiProgress::with_draw_target(target),
            look,
            tick,
            columns,
            state: Mutex::new(State::default()),
        }
    }

    pub(crate) fn stderr(look: Look) -> TtyProgress {
        let columns = console::Term::stderr()
            .size_checked()
            .map_or(80, |(_rows, columns)| columns);
        TtyProgress::new(
            ProgressDrawTarget::stderr_with_hz(REFRESH_HZ),
            look,
            Some(Duration::from_millis(120)),
            columns,
        )
    }

    fn print(&self, line: &str) {
        // A terminal that cannot be written has nobody to tell.
        let _ = self.multi.println(line);
    }

    fn style(&self, total: Total) -> ProgressStyle {
        let template = match (total, self.columns >= BAR_COLUMNS) {
            (Total::Unknown, _) => "{spinner} {prefix}{msg}",
            (Total::Bytes(_), true) => "  {prefix}{msg} [{bar:10}] {human}",
            (Total::Bytes(_), false) => "  {prefix}{msg} {percent:>3}%",
            (Total::Items(_), true) => "  {prefix}{msg} [{bar:10}] {pos}/{len}",
            (Total::Items(_), false) => "  {prefix}{msg} {pos}/{len}",
        };
        ProgressStyle::with_template(template)
            .expect("the template is valid")
            .tick_chars("-\\|/ ")
            .with_key("human", |state: &ProgressState, out: &mut dyn std::fmt::Write| {
                let _ = write!(
                    out,
                    "{}/{}",
                    human_bytes(state.pos()),
                    human_bytes(state.len().unwrap_or(0))
                );
            })
    }

    fn begin(&self, state: &mut State, id: TaskId, parent: Option<TaskId>, label: &str, total: Total) {
        let label = escape_controls(label).into_owned();
        let parent_at = parent.and_then(|parent| state.order.iter().position(|open| *open == parent));
        let (depth, index) = match parent_at {
            Some(at) => {
                let depth = state.tasks[&state.order[at]].depth + 1;
                // After the last descendant of the parent: the next task that is not deeper than it.
                let end = state.order[at + 1..]
                    .iter()
                    .position(|open| state.tasks[open].depth < depth)
                    .map_or(state.order.len(), |offset| at + 1 + offset);
                (depth, end)
            }
            None => (0, state.order.len()),
        };
        let bar = match total {
            Total::Unknown => ProgressBar::new_spinner(),
            Total::Bytes(size) | Total::Items(size) => ProgressBar::new(size),
        };
        bar.set_style(self.style(total));
        let bar = self.multi.insert(index, bar);
        if let (Total::Unknown, Some(tick)) = (total, self.tick) {
            bar.enable_steady_tick(tick);
        }
        state.order.insert(index, id);
        state.tasks.insert(
            id,
            Slot {
                bar,
                label,
                status: None,
                total,
                depth,
            },
        );
        self.relabel(state);
    }

    /// Sets each bar's tree prefix from the order and the depths, and its text to fit what is left of the line; a
    /// sibling that ends changes its elder's glyph.
    fn relabel(&self, state: &State) {
        for (at, id) in state.order.iter().enumerate() {
            self.refresh(&state.tasks[id], &prefix(state, at));
        }
    }

    /// Draws `slot` with the tree `prefix`. The label is cut so that the line is shorter than the terminal: a line that
    /// fills it wraps on some terminals, and a redraw clears one row of the two.
    fn refresh(&self, slot: &Slot, prefix: &str) {
        let text = match &slot.status {
            Some(status) => format!("{}: {status}", slot.label),
            None => slot.label.clone(),
        };
        let used = 2 + console::measure_text_width(prefix) + self.suffix_width(slot.total) + 1;
        let budget = usize::from(self.columns).saturating_sub(used).max(1);
        slot.bar.set_prefix(prefix.to_owned());
        slot.bar
            .set_message(console::truncate_str(&text, budget, "…").into_owned());
    }

    /// The most columns the bar and the position take after the label.
    fn suffix_width(&self, total: Total) -> usize {
        match (total, self.columns >= BAR_COLUMNS) {
            (Total::Unknown, _) => 0,
            // ` [` bar `] ` and `1023.9 MiB/1023.9 GiB`.
            (Total::Bytes(_), true) => 14 + 21,
            (Total::Items(_), true) => 14 + 21,
            // ` 100%`, ` 18446744073709551615/18446744073709551615`.
            (Total::Bytes(_), false) => 5,
            (Total::Items(_), false) => 42,
        }
    }

    fn end(&self, state: &mut State, id: TaskId) {
        let Some(at) = state.order.iter().position(|open| *open == id) else {
            return;
        };
        state.order.remove(at);
        let Some(slot) = state.tasks.remove(&id) else { return };
        slot.bar.finish_and_clear();
        self.multi.remove(&slot.bar);
        self.relabel(state);
    }
}

/// The tree glyphs before the task at `at`: `│  ` or spaces for each ancestor, then `├─ ` or `└─ `.
fn prefix(state: &State, at: usize) -> String {
    let depth_of = |index: usize| state.tasks[&state.order[index]].depth;
    let depth = depth_of(at);
    if depth == 0 {
        return String::new();
    }
    // Whether the task at `index` has a later sibling: a later task of its depth before one that is shallower.
    let has_later_sibling = |index: usize| {
        let depth = depth_of(index);
        (index + 1..state.order.len())
            .map(depth_of)
            .find(|other| *other <= depth)
            .is_some_and(|other| other == depth)
    };
    let mut text = String::new();
    for level in 1..depth {
        // The ancestor of the task at that level: the nearest earlier task of that depth.
        let ancestor = (0..at)
            .rev()
            .find(|index| depth_of(*index) == level)
            .expect("a parent precedes");
        text.push_str(if has_later_sibling(ancestor) { "│  " } else { "   " });
    }
    text.push_str(if has_later_sibling(at) { "├─ " } else { "└─ " });
    text
}

impl Progress for TtyProgress {
    fn event(&self, event: &Event) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let state = &mut *state;
        match event {
            Event::Note { level, text } => {
                let text = escape_controls(text);
                match level {
                    Level::Debug if !self.look.verbose => {}
                    Level::Warn => {
                        let word = paint(self.look.colour, Colour::Yellow, "warning:");
                        self.print(&format!("{word} {text}"));
                    }
                    Level::Debug | Level::Info => self.print(&text),
                }
            }
            Event::Output { line } => {
                if self.look.verbose {
                    let plain = console::strip_ansi_codes(line);
                    self.print(&escape_controls(&plain));
                }
            }
            Event::Begin {
                id,
                parent,
                label,
                total,
            } => self.begin(state, *id, *parent, label, *total),
            Event::Status { id, text } => {
                if let Some(at) = state.order.iter().position(|open| open == id) {
                    let status = escape_controls(text).into_owned();
                    if let Some(slot) = state.tasks.get_mut(id) {
                        slot.status = Some(status);
                    }
                    self.refresh(&state.tasks[id], &prefix(state, at));
                }
            }
            Event::Advance { id, position } => {
                if let Some(slot) = state.tasks.get(id) {
                    slot.bar.set_position(*position);
                }
            }
            // Finished or not, the task leaves no line: an error is reported below the bars.
            Event::End { id, .. } => self.end(state, *id),
        }
    }
}
