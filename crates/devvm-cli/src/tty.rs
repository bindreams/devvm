//! Progress bars and a task tree on a terminal.

use std::collections::{HashMap, HashSet};
use std::io::StderrLock;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use devvm::progress::{Colour, Event, Level, Look, Progress, TaskId, Total, escape_controls, human_bytes, paint};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};

#[cfg(test)]
#[path = "tty_tests.rs"]
mod tty_tests;

/// Below this many columns a sized task shows no bar: its line would wrap.
const BAR_COLUMNS: u16 = 50;

/// Draws one line per open task, children under their parent with `├─` and `└─`, and prints notes above them.
///
/// A task that ends leaves no line. A sized task draws a bar and its position in the units of
/// [`devvm::progress::human_bytes`]; a task of unknown size is a spinner, with the bytes it has counted. A `Begin`
/// whose parent this screen never saw is drawn as a root. Each label is cut to the terminal's width when its line is
/// set: at each report of its task.
///
/// Every call into indicatif is made holding the screen's lock and then stderr's, and nothing else draws: the spinners
/// are ticked by a thread of this screen that takes the same locks. Stderr's is the lock an ending writes under, and a
/// cursor move on a Windows console takes none of its own. Every report checks the screen's `stopped` flag under the
/// screen's lock, so a stopped screen ([`TtyProgress::stop`]) draws nothing more. A thread that panics inside a draw
/// leaves the lock poisoned, and the screen is then never touched again: what indicatif holds may be half changed, and
/// dropping it would panic.
pub(crate) struct TtyProgress {
    /// Stops the spinners' thread when this screen is dropped. Declared first, so that it ends before the rest.
    _ticker: Option<Ticker>,
    shared: Arc<Shared>,
}

struct Shared {
    /// `None` only for [`Shared`]'s drop, which leaves a poisoned screen as it is.
    screen: Mutex<Option<Screen>>,
    pace: Arc<Pace>,
}

struct Screen {
    multi: MultiProgress,
    look: Look,
    /// Set by [`TtyProgress::stop`]: nothing is drawn after it.
    stopped: bool,
    columns: Box<dyn Fn() -> u16 + Send + Sync>,
    /// The open tasks in the order they are drawn: a parent, then each of its children with their own.
    order: Vec<TaskId>,
    tasks: HashMap<TaskId, Slot>,
    /// The open tasks of unknown size, which the ticker turns.
    spinners: usize,
    pace: Arc<Pace>,
}

struct Slot {
    bar: ProgressBar,
    label: String,
    status: Option<String>,
    total: Total,
    /// The tasks this one is below, the root first, as they were when it began. One that has ended is no longer in
    /// [`Screen::tasks`].
    ancestors: Vec<TaskId>,
    position: u64,
    /// The tree glyphs before the label, and their width.
    prefix: String,
    prefix_width: usize,
    /// What the bar's style and message were last set for; `None` before the first time.
    fit: Option<Fit>,
    /// The prefix, label or status changed since the bar was given its text.
    stale: bool,
}

#[derive(Clone, Copy)]
struct Fit {
    /// The terminal is wide enough for a bar.
    wide: bool,
    /// The columns the label and status may take.
    budget: usize,
}

impl Slot {
    fn parent(&self) -> Option<TaskId> {
        self.ancestors.last().copied()
    }
}

// Locking =============================================================================================================

/// The screen, locked and not stopped, and stderr's lock with it.
struct Locked<'a> {
    screen: MutexGuard<'a, Option<Screen>>,
    _stderr: StderrLock<'static>,
}

impl std::ops::Deref for Locked<'_> {
    type Target = Screen;

    fn deref(&self) -> &Screen {
        self.screen.as_ref().expect("the screen is there until it is dropped")
    }
}

impl std::ops::DerefMut for Locked<'_> {
    fn deref_mut(&mut self) -> &mut Screen {
        self.screen.as_mut().expect("the screen is there until it is dropped")
    }
}

impl Shared {
    /// The screen, locked; `None` when a thread panicked inside a draw, and when the screen is stopped. Stderr's lock is
    /// taken after the screen's.
    fn lock(&self) -> Option<Locked<'_>> {
        let screen = self.screen.lock().ok()?;
        let open = screen.as_ref().is_some_and(|screen| !screen.stopped);
        open.then(|| Locked {
            screen,
            _stderr: std::io::stderr().lock(),
        })
    }

    /// Turns each spinner once.
    fn tick(&self) {
        let Some(screen) = self.lock() else { return };
        for slot in screen.tasks.values() {
            if matches!(slot.total, Total::Unknown) {
                slot.bar.tick();
            }
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // The bars of a screen that a panic poisoned would panic when dropped.
        if self.screen.is_poisoned()
            && let Some(screen) = self.screen.get_mut().unwrap_or_else(PoisonError::into_inner).take()
        {
            std::mem::forget(screen);
        }
    }
}

// Ticking =============================================================================================================

/// When the spinners turn: while there is one, until stopped.
#[derive(Default)]
struct Pace {
    state: Mutex<PaceState>,
    changed: Condvar,
}

#[derive(Default)]
struct PaceState {
    spinners: usize,
    stopped: bool,
}

impl Pace {
    fn state(&self) -> MutexGuard<'_, PaceState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_spinners(&self, spinners: usize) {
        self.state().spinners = spinners;
        self.changed.notify_all();
    }

    fn stop(&self) {
        self.state().stopped = true;
        self.changed.notify_all();
    }

    /// Waits until a spinner is to be turned, which is `every` after the last turn; false once stopped. With no spinner
    /// it waits for one, and wakes for nothing else.
    fn wait_turn(&self, every: Duration) -> bool {
        let mut state = self.state();
        while !state.stopped && state.spinners == 0 {
            state = self.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
        // The wait sets the pace of the turns; a stop or the last spinner's end cuts it short.
        let (state, _) = self
            .changed
            .wait_timeout_while(state, every, |state| !state.stopped && state.spinners > 0)
            .unwrap_or_else(PoisonError::into_inner);
        !state.stopped
    }
}

/// The thread that turns the spinners.
struct Ticker {
    pace: Arc<Pace>,
    thread: Option<JoinHandle<()>>,
}

impl Ticker {
    /// A spinner that cannot be ticked stands still; the screen is otherwise right, so there is nobody to tell.
    fn start(shared: Arc<Shared>, every: Duration) -> Option<Ticker> {
        let pace = shared.pace.clone();
        let thread = thread::Builder::new()
            .name("devvm-spinners".to_owned())
            .spawn(move || {
                while shared.pace.wait_turn(every) {
                    shared.tick();
                }
            })
            .ok()?;
        Some(Ticker {
            pace,
            thread: Some(thread),
        })
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.pace.stop();
        if let Some(thread) = self.thread.take() {
            // A thread that panicked has said so through the panic hook.
            let _ = thread.join();
        }
    }
}

// The screen ==========================================================================================================

impl TtyProgress {
    /// Draws to `target`, which `columns` says is that wide now; spinners turn each `tick`, or only when their task
    /// reports, with `None`.
    pub(crate) fn new(
        target: ProgressDrawTarget,
        look: Look,
        tick: Option<Duration>,
        columns: impl Fn() -> u16 + Send + Sync + 'static,
    ) -> TtyProgress {
        let pace = Arc::new(Pace::default());
        let shared = Arc::new(Shared {
            screen: Mutex::new(Some(Screen {
                multi: MultiProgress::with_draw_target(target),
                look,
                stopped: false,
                columns: Box::new(columns),
                order: Vec::new(),
                tasks: HashMap::new(),
                spinners: 0,
                pace: pace.clone(),
            })),
            pace,
        });
        let ticker = tick.and_then(|every| Ticker::start(shared.clone(), every));
        TtyProgress {
            _ticker: ticker,
            shared,
        }
    }

    pub(crate) fn stderr(look: Look) -> TtyProgress {
        TtyProgress::new(stderr_target(), look, Some(Duration::from_millis(120)), || {
            console::Term::stderr()
                .size_checked()
                .map_or(80, |(_rows, columns)| columns)
        })
    }

    /// Takes the screen's lock, which waits for the draw in flight, stops the screen and takes the bars off under that
    /// lock: nothing is drawn after it, and the cursor is where the first bar was. A screen that a panic poisoned is
    /// left as it is.
    pub(crate) fn stop(&self) {
        self.shared.pace.stop();
        let Ok(mut screen) = self.shared.screen.lock() else {
            return;
        };
        let _stderr = std::io::stderr().lock();
        if let Some(screen) = screen.as_mut() {
            screen.stopped = true;
            let _ = screen.multi.clear();
            screen.multi.set_draw_target(ProgressDrawTarget::hidden());
        }
    }
}

/// The terminal's draw target, with no rate limit: every report is drawn. Reports are throttled where they are made.
fn stderr_target() -> ProgressDrawTarget {
    let term = console::Term::buffered_stderr();
    if term.is_term() {
        ProgressDrawTarget::term_like(Box::new(term))
    } else {
        ProgressDrawTarget::hidden()
    }
}

/// The most columns the bar and the position of a task of `total` take after its label, with the task at `position`.
fn suffix_width(total: Total, wide: bool, position: u64) -> usize {
    let pair = |items: u64| 2 * digits(items.max(position)) + 1;
    // The widest of each: ` [bar] 1023.9 MiB/1023.9 GiB`, ` 100%`, ` 12/3456`.
    match (total, wide) {
        (Total::Unknown, _) if position > 0 => 1 + human_bytes(position).len(),
        (Total::Unknown, _) => 0,
        (Total::Bytes(_), true) => 14 + 21,
        (Total::Bytes(_), false) => 5,
        (Total::Items(items), true) => 14 + pair(items),
        (Total::Items(items), false) => 1 + pair(items),
    }
}

fn digits(number: u64) -> usize {
    number.checked_ilog10().map_or(1, |log| log as usize + 1)
}

fn style(total: Total, wide: bool) -> ProgressStyle {
    let template = match (total, wide) {
        (Total::Unknown, _) => "{spinner} {prefix}{msg}{counted}",
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
        // The bytes a task of unknown size has counted: nothing before its first position.
        .with_key("counted", |state: &ProgressState, out: &mut dyn std::fmt::Write| {
            if state.pos() > 0 {
                let _ = write!(out, " {}", human_bytes(state.pos()));
            }
        })
}

impl Screen {
    fn print(&self, line: &str) {
        // A terminal that cannot be written has nobody to tell.
        let _ = self.multi.println(line);
    }

    fn begin(&mut self, id: TaskId, parent: Option<TaskId>, label: &str, total: Total) {
        let label = escape_controls(label).into_owned();
        // A parent that is not open (never seen, or ended) makes a root.
        let parent = parent.filter(|parent| self.tasks.contains_key(parent));
        let ancestors = parent.map_or_else(Vec::new, |parent| {
            let mut chain = self.tasks[&parent].ancestors.clone();
            chain.push(parent);
            chain
        });
        let index = match parent {
            // Below the last descendant of the parent.
            Some(parent) => {
                let at = self.order.iter().position(|open| *open == parent).unwrap_or(0);
                self.order
                    .iter()
                    .rposition(|open| self.tasks[open].ancestors.contains(&parent))
                    .map_or(at + 1, |last| last + 1)
            }
            None => self.order.len(),
        };
        let bar = match total {
            Total::Unknown => ProgressBar::new_spinner(),
            Total::Bytes(size) | Total::Items(size) => ProgressBar::new(size),
        };
        let bar = self.multi.insert(index, bar);
        self.order.insert(index, id);
        if matches!(total, Total::Unknown) {
            self.spinners += 1;
            self.pace.set_spinners(self.spinners);
        }
        self.tasks.insert(
            id,
            Slot {
                bar,
                label,
                status: None,
                total,
                ancestors,
                position: 0,
                prefix: String::new(),
                prefix_width: 0,
                fit: None,
                stale: true,
            },
        );
        // The new line is styled before any other line's change draws the screen: it has no style of its own yet, and
        // the default one fills the width of the terminal.
        let columns = (self.columns)();
        self.relabel(columns, Some(index));
    }

    fn end(&mut self, id: TaskId) {
        let Some(at) = self.order.iter().position(|open| *open == id) else {
            return;
        };
        self.order.remove(at);
        let Some(slot) = self.tasks.remove(&id) else { return };
        if matches!(slot.total, Total::Unknown) {
            self.spinners -= 1;
            self.pace.set_spinners(self.spinners);
        }
        slot.bar.finish_and_clear();
        self.multi.remove(&slot.bar);
        // A sibling that ends changes its elder's glyph.
        self.relabel((self.columns)(), None);
    }

    /// Sets each line's tree prefix from the order and the ancestry, and refreshes it. The line at `first` goes first.
    fn relabel(&mut self, columns: u16, first: Option<usize>) {
        let prefixes = self.prefixes();
        let rest = (0..prefixes.len()).filter(|index| Some(*index) != first);
        for index in first.into_iter().chain(rest) {
            let id = self.order[index];
            let slot = self.tasks.get_mut(&id).expect("an open task has a slot");
            if slot.prefix != prefixes[index] {
                slot.prefix_width = console::measure_text_width(&prefixes[index]);
                slot.prefix.clone_from(&prefixes[index]);
                slot.stale = true;
            }
            self.refresh(id, columns);
        }
    }

    /// The tree glyphs before each open task: `│  ` or spaces for each ancestor below the root, then `├─ ` or `└─ `. A
    /// column goes on (`│`) while a row below has the same parent as that ancestor, so an ancestor that has ended keeps
    /// its column for the siblings it left.
    fn prefixes(&self) -> Vec<String> {
        let mut below: HashSet<Option<TaskId>> = HashSet::new();
        let mut prefixes = vec![String::new(); self.order.len()];
        for (index, id) in self.order.iter().enumerate().rev() {
            let slot = &self.tasks[id];
            if !slot.ancestors.is_empty() {
                let mut text = String::new();
                for elder in &slot.ancestors[..slot.ancestors.len() - 1] {
                    text.push_str(if below.contains(&Some(*elder)) { "│  " } else { "   " });
                }
                text.push_str(if below.contains(&slot.parent()) {
                    "├─ "
                } else {
                    "└─ "
                });
                prefixes[index] = text;
            }
            below.insert(slot.parent());
        }
        prefixes
    }

    /// Gives the bar of `id` the style and the text that fit a terminal `columns` wide, where something changed. The
    /// label is cut so that the line is shorter than the terminal: a line that fills it wraps on some terminals, and a
    /// redraw clears one row of the two.
    fn refresh(&mut self, id: TaskId, columns: u16) {
        let Some(slot) = self.tasks.get_mut(&id) else { return };
        let wide = columns >= BAR_COLUMNS;
        let used = 2 + slot.prefix_width + suffix_width(slot.total, wide, slot.position) + 1;
        let budget = usize::from(columns).saturating_sub(used).max(1);
        let was = slot.fit;
        if was.map(|fit| fit.wide) != Some(wide) {
            slot.bar.set_style(style(slot.total, wide));
        }
        if slot.stale {
            slot.bar.set_prefix(slot.prefix.clone());
        }
        if slot.stale || was.map(|fit| fit.budget) != Some(budget) {
            let text = match &slot.status {
                Some(status) => format!("{}: {status}", slot.label),
                None => slot.label.clone(),
            };
            slot.bar
                .set_message(console::truncate_str(&text, budget, "…").into_owned());
        }
        slot.stale = false;
        slot.fit = Some(Fit { wide, budget });
    }
}

impl Progress for TtyProgress {
    fn event(&self, event: &Event) {
        let Some(mut screen) = self.shared.lock() else { return };
        let screen = &mut *screen;
        match event {
            Event::Note { level, text } => {
                let text = escape_controls(text);
                match level {
                    Level::Debug if !screen.look.verbose => {}
                    Level::Warn => {
                        let word = paint(screen.look.colour, Colour::Yellow, "warning:");
                        screen.print(&format!("{word} {text}"));
                    }
                    Level::Debug | Level::Info => screen.print(&text),
                }
            }
            Event::Output { line } => {
                if screen.look.verbose {
                    screen.print(&escape_controls(line));
                }
            }
            Event::Begin {
                id,
                parent,
                label,
                total,
            } => screen.begin(*id, *parent, label, *total),
            Event::Status { id, text } => {
                let columns = (screen.columns)();
                if let Some(slot) = screen.tasks.get_mut(id) {
                    slot.status = Some(escape_controls(text).into_owned());
                    slot.stale = true;
                    screen.refresh(*id, columns);
                }
            }
            Event::Advance { id, position } => {
                let columns = (screen.columns)();
                if let Some(slot) = screen.tasks.get_mut(id) {
                    slot.position = *position;
                    // The text that fits the new position goes first, so that no line is drawn wider than the
                    // terminal.
                    screen.refresh(*id, columns);
                    screen.tasks[id].bar.set_position(*position);
                }
            }
            // Finished or not, the task leaves no line: an error is reported below the bars.
            Event::End { id, .. } => screen.end(*id),
        }
    }
}
