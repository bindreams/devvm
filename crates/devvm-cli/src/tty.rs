//! Progress bars and a task tree on a terminal.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle, ThreadId};
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
/// [`devvm::progress::human_bytes`]; a task of unknown size is a spinner, with the bytes it has counted. A `Begin`
/// whose parent this screen never saw is drawn as a root. Each label is cut to the terminal's width when its line is
/// set: at each report of its task.
///
/// Every call into indicatif is made holding one lock, the screen's, and nothing else draws: the spinners are ticked
/// by a thread of this screen that takes the same lock. So [`TtyProgress::quiet`] ends all drawing at once.
pub(crate) struct TtyProgress {
    shared: Arc<Shared>,
    /// Stops the spinners' thread when this screen is dropped.
    _ticker: Option<Ticker>,
}

struct Shared {
    /// Every call into indicatif is made holding this.
    screen: Mutex<Screen>,
    /// The thread that holds `screen`, so that a panic inside a draw does not wait on itself.
    owner: Mutex<Option<ThreadId>>,
    /// Nothing is drawn any more: set before `screen` is taken, so a draw that waits for it draws nothing.
    quiet: AtomicBool,
    /// Starts a fresh line below the bars, for a thread that cannot take `screen`.
    newline: Box<dyn Fn() + Send + Sync>,
}

struct Screen {
    multi: MultiProgress,
    look: Look,
    /// The terminal's width now.
    columns: Box<dyn Fn() -> u16 + Send + Sync>,
    /// The open tasks in the order they are drawn: a parent, then each of its children with their own.
    order: Vec<TaskId>,
    tasks: HashMap<TaskId, Slot>,
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

/// The screen, locked; records the thread that holds it.
struct Held<'a> {
    screen: MutexGuard<'a, Screen>,
    owner: &'a Mutex<Option<ThreadId>>,
}

impl std::ops::Deref for Held<'_> {
    type Target = Screen;

    fn deref(&self) -> &Screen {
        &self.screen
    }
}

impl std::ops::DerefMut for Held<'_> {
    fn deref_mut(&mut self) -> &mut Screen {
        &mut self.screen
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        *self.owner.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

impl Shared {
    /// A thread that panicked inside a draw is poisoning the lock it holds: later calls go on.
    fn lock(&self) -> Held<'_> {
        let screen = self.screen.lock().unwrap_or_else(PoisonError::into_inner);
        *self.owner.lock().unwrap_or_else(PoisonError::into_inner) = Some(thread::current().id());
        Held {
            screen,
            owner: &self.owner,
        }
    }

    /// Whether this thread holds the screen lock: only it can set or clear its own id there.
    fn held_by_this_thread(&self) -> bool {
        *self.owner.lock().unwrap_or_else(PoisonError::into_inner) == Some(thread::current().id())
    }

    fn is_quiet(&self) -> bool {
        self.quiet.load(Ordering::SeqCst)
    }

    /// Turns each spinner once.
    fn tick(&self) {
        let screen = self.lock();
        if self.is_quiet() {
            return;
        }
        for slot in screen.tasks.values() {
            if matches!(slot.total, Total::Unknown) {
                slot.bar.tick();
            }
        }
    }
}

// Ticking =============================================================================================================

/// The thread that turns the spinners.
struct Ticker {
    /// Dropped to stop the thread.
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Ticker {
    /// A spinner that cannot be ticked stands still; the screen is otherwise right, so there is nobody to tell.
    fn start(shared: Arc<Shared>, every: Duration) -> Option<Ticker> {
        let (stop, stopped) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("devvm-spinners".to_owned())
            .spawn(move || {
                // The wait sets the pace of the ticks; the sender's drop is what ends the loop.
                while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(every) {
                    shared.tick();
                }
            })
            .ok()?;
        Some(Ticker {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            // A thread that panicked has said so through the panic hook.
            let _ = thread.join();
        }
    }
}

// The screen ==========================================================================================================

impl TtyProgress {
    /// Draws to `target`, which `columns` says is that wide now; spinners turn each `tick`, or only when their task
    /// reports, with `None`. `newline` starts a fresh line on the terminal.
    pub(crate) fn new(
        target: ProgressDrawTarget,
        look: Look,
        tick: Option<Duration>,
        columns: impl Fn() -> u16 + Send + Sync + 'static,
        newline: impl Fn() + Send + Sync + 'static,
    ) -> TtyProgress {
        let shared = Arc::new(Shared {
            screen: Mutex::new(Screen {
                multi: MultiProgress::with_draw_target(target),
                look,
                columns: Box::new(columns),
                order: Vec::new(),
                tasks: HashMap::new(),
            }),
            owner: Mutex::new(None),
            quiet: AtomicBool::new(false),
            newline: Box::new(newline),
        });
        let ticker = tick.and_then(|every| Ticker::start(shared.clone(), every));
        TtyProgress {
            shared,
            _ticker: ticker,
        }
    }

    pub(crate) fn stderr(look: Look) -> TtyProgress {
        TtyProgress::new(
            ProgressDrawTarget::stderr_with_hz(REFRESH_HZ),
            look,
            Some(Duration::from_millis(120)),
            || {
                console::Term::stderr()
                    .size_checked()
                    .map_or(80, |(_rows, columns)| columns)
            },
            || {
                // A terminal that cannot be written has nobody to tell.
                let _ = console::Term::stderr().write_line("");
            },
        )
    }

    /// Stops drawing and takes the bars off the screen, so that what is written below next stays: no later draw, a
    /// spinner's tick included, can erase it. A draw of another thread is over when this returns.
    ///
    /// A thread that panicked inside [`Progress::event`] holds the screen lock and cannot take it: it starts a fresh
    /// line and leaves the screen as it is. A panicking thread draws nothing (indicatif skips it), so it starts a fresh
    /// line below the bars too.
    pub(crate) fn quiet(&self) {
        let shared = &*self.shared;
        shared.quiet.store(true, Ordering::SeqCst);
        if shared.held_by_this_thread() {
            (shared.newline)();
            return;
        }
        let screen = shared.lock();
        // A terminal that cannot be written has nobody to tell.
        let _ = screen.multi.clear();
        screen.multi.set_draw_target(ProgressDrawTarget::hidden());
        if thread::panicking() {
            (shared.newline)();
        }
    }
}

/// The most columns the bar and the position of a task of `total` take after its label, with the task at `position`.
fn suffix_width(total: Total, wide: bool, position: u64) -> usize {
    // A position and a total of a count, `12/3456`: the most digits either has.
    let pair = |items: u64| 2 * digits(items.max(position)) + 1;
    match (total, wide) {
        // ` 5.0 MiB`, once the task has counted bytes.
        (Total::Unknown, _) if position > 0 => 1 + human_bytes(position).len(),
        (Total::Unknown, _) => 0,
        // ` [` bar `] ` and `1023.9 MiB/1023.9 GiB`.
        (Total::Bytes(_), true) => 14 + 21,
        // ` 100%`.
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
        self.set_prefix(index);
        self.refresh(id, columns);
        self.relabel(columns);
    }

    fn end(&mut self, id: TaskId) {
        let Some(at) = self.order.iter().position(|open| *open == id) else {
            return;
        };
        self.order.remove(at);
        let Some(slot) = self.tasks.remove(&id) else { return };
        slot.bar.finish_and_clear();
        self.multi.remove(&slot.bar);
        // A sibling that ends changes its elder's glyph.
        self.relabel((self.columns)());
    }

    /// Sets each line's tree prefix from the order and the ancestry, and refreshes it.
    fn relabel(&mut self, columns: u16) {
        for index in 0..self.order.len() {
            self.set_prefix(index);
            let id = self.order[index];
            self.refresh(id, columns);
        }
    }

    fn set_prefix(&mut self, index: usize) {
        let prefix = self.prefix(index);
        let slot = self.tasks.get_mut(&self.order[index]).expect("an open task has a slot");
        if slot.prefix != prefix {
            slot.prefix_width = console::measure_text_width(&prefix);
            slot.prefix = prefix;
            slot.stale = true;
        }
    }

    /// Whether the task at `index` has a later sibling: a later task with the same parent.
    fn has_later_sibling(&self, index: usize) -> bool {
        let parent = self.tasks[&self.order[index]].parent();
        self.order[index + 1..]
            .iter()
            .any(|open| self.tasks[open].parent() == parent)
    }

    /// The tree glyphs before the task at `index`: `│  ` or spaces for each ancestor below the root, then `├─ ` or
    /// `└─ `. An ancestor that has ended leaves a blank column.
    fn prefix(&self, index: usize) -> String {
        let slot = &self.tasks[&self.order[index]];
        if slot.ancestors.is_empty() {
            return String::new();
        }
        let mut text = String::new();
        for ancestor in &slot.ancestors[1..] {
            let continues = self
                .order
                .iter()
                .position(|open| open == ancestor)
                .is_some_and(|at| self.has_later_sibling(at));
            text.push_str(if continues { "│  " } else { "   " });
        }
        text.push_str(if self.has_later_sibling(index) {
            "├─ "
        } else {
            "└─ "
        });
        text
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
        let mut screen = self.shared.lock();
        if self.shared.is_quiet() {
            return;
        }
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
