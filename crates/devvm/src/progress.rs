//! What a running command tells the person: notes, waits and tasks with a position.
//!
//! The library reports [`Event`]s to a [`Progress`] sink and never draws anything itself.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

mod lines;
mod log;
pub use lines::{LineProgress, Look, VERBOSE_VAR, verbose_from_env};
pub use log::FileLog;

#[cfg(test)]
#[path = "progress_tests.rs"]
mod progress_tests;

/// How much a note matters to the person reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// A fact only someone reading the log wants.
    Debug,
    /// A state change the person should see.
    Info,
    /// The command goes on past a failure or a skipped thing.
    Warn,
}

/// What a task counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Total {
    /// The size is not known. A position such a task reports counts bytes.
    Unknown,
    Bytes(u64),
    Items(u64),
}

/// Names a task in the events it emits. Unique within the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(u64);

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// How a task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Finished,
    /// Ended by an error or an early return.
    Abandoned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Note {
        level: Level,
        text: String,
    },
    /// A line a step's command printed, without ANSI escapes.
    Output {
        line: String,
    },
    Begin {
        id: TaskId,
        parent: Option<TaskId>,
        label: String,
        total: Total,
    },
    Status {
        id: TaskId,
        text: String,
    },
    /// The task's position: bytes or items so far.
    Advance {
        id: TaskId,
        position: u64,
    },
    End {
        id: TaskId,
        outcome: Outcome,
    },
}

/// Receives what a command reports while it runs.
///
/// A sink that receives a `Begin` whose `parent` it never saw (it was attached after the parent began) treats the task
/// as a root.
pub trait Progress: Send + Sync {
    fn event(&self, event: &Event);

    fn message(&self, text: &str) {
        self.event(&Event::Note {
            level: Level::Info,
            text: text.to_owned(),
        });
    }

    fn warn(&self, text: &str) {
        self.event(&Event::Note {
            level: Level::Warn,
            text: text.to_owned(),
        });
    }

    fn debug(&self, text: &str) {
        self.event(&Event::Note {
            level: Level::Debug,
            text: text.to_owned(),
        });
    }

    /// A line a step's command printed. This is the one place an [`Event::Output`] is built: the line loses its ANSI
    /// escapes here, so every sink gets plain text.
    fn output(&self, line: &str) {
        self.event(&Event::Output {
            line: strip_ansi(line).into_owned(),
        });
    }
}

impl dyn Progress + '_ {
    /// Begins a task; it ends when the guard is finished or dropped.
    pub fn task(&self, label: impl Into<String>, total: Total) -> Task<'_> {
        Task::begin(self, label, total)
    }
}

/// Discards everything.
pub struct NullProgress;

impl Progress for NullProgress {
    fn event(&self, _event: &Event) {}
}

/// Forwards each event to every sink, in order.
pub struct Fanout(Vec<Arc<dyn Progress>>);

impl Fanout {
    pub fn new(sinks: Vec<Arc<dyn Progress>>) -> Fanout {
        Fanout(sinks)
    }
}

impl Progress for Fanout {
    fn event(&self, event: &Event) {
        for sink in &self.0 {
            sink.event(event);
        }
    }
}

// Tasks ===============================================================================================================

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The tasks open on this thread, innermost last. A new task's parent is the top.
    static OPEN: RefCell<Vec<TaskId>> = const { RefCell::new(Vec::new()) };
}

const MIB: u64 = 1 << 20;

/// An open task. Nesting follows the call stack: a task begun while another is open on the same thread is its child,
/// and a task begun on a thread with none open is a root. Ends [`Outcome::Abandoned`] when dropped without
/// [`Task::finish`].
///
/// Not `Send`: a guard ends on the thread that began it. Guards held in one struct drop in field order, so declare the
/// inner task first; out of order, a debug build panics on drop (never while already unwinding from another panic).
pub struct Task<'a> {
    progress: &'a dyn Progress,
    id: TaskId,
    total: Total,
    last: Cell<u64>,
    reported: Cell<bool>,
    outcome: Outcome,
    _same_thread: PhantomData<*const ()>,
}

impl<'a> Task<'a> {
    pub fn begin(progress: &'a dyn Progress, label: impl Into<String>, total: Total) -> Task<'a> {
        let id = TaskId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let parent = OPEN.with(|open| {
            let mut open = open.borrow_mut();
            let parent = open.last().copied();
            open.push(id);
            parent
        });
        progress.event(&Event::Begin {
            id,
            parent,
            label: label.into(),
            total,
        });
        Task {
            progress,
            id,
            total,
            last: Cell::new(0),
            reported: Cell::new(false),
            outcome: Outcome::Abandoned,
            _same_thread: PhantomData,
        }
    }

    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Reports the position, which never goes back. Bytes are reported once per MiB crossed and at the total; items
    /// each time they grow.
    pub fn set(&self, position: u64) {
        let last = self.last.get();
        if position <= last {
            return;
        }
        let due = match self.total {
            Total::Bytes(total) => !self.reported.get() || position / MIB > last / MIB || position >= total,
            Total::Unknown => !self.reported.get() || position / MIB > last / MIB,
            Total::Items(_) => true,
        };
        if due {
            self.last.set(position);
            self.reported.set(true);
            self.progress.event(&Event::Advance { id: self.id, position });
        }
    }

    pub fn status(&self, text: impl Into<String>) {
        self.progress.event(&Event::Status {
            id: self.id,
            text: text.into(),
        });
    }

    pub fn finish(mut self) {
        self.outcome = Outcome::Finished;
    }
}

impl Drop for Task<'_> {
    fn drop(&mut self) {
        OPEN.with(|open| {
            let mut open = open.borrow_mut();
            if !std::thread::panicking() {
                debug_assert_eq!(open.last(), Some(&self.id), "tasks end innermost first");
            }
            open.retain(|id| *id != self.id);
        });
        self.progress.event(&Event::End {
            id: self.id,
            outcome: self.outcome,
        });
    }
}

/// A byte count in binary units, as every renderer shows it: `512.0 MiB`, `4.8 GiB`.
pub fn human_bytes(bytes: u64) -> String {
    bytesize::ByteSize(bytes).display().iec().to_string()
}

/// A colour [`paint`] knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colour {
    Red,
    Green,
    Yellow,
}

/// `text` in `colour` when `on`, else as it is. The one place devvm writes a colour escape.
pub fn paint(on: bool, colour: Colour, text: &str) -> String {
    if !on {
        return text.to_owned();
    }
    let code = match colour {
        Colour::Red => "31",
        Colour::Green => "32",
        Colour::Yellow => "33",
    };
    format!("\u{1b}[{code}m{text}\u{1b}[0m")
}

/// `text` without ANSI escape sequences (colours, cursor moves), which a tool prints for a terminal.
pub fn strip_ansi(text: &str) -> Cow<'_, str> {
    console::strip_ansi_codes(text)
}

/// `text` with each control character written as an escape (`\r`, `\u{1b}`), so it stays on one line and cannot move a
/// terminal's cursor. Everything else, a backslash included, is unchanged.
pub fn escape_controls(text: &str) -> Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return Cow::Borrowed(text);
    }
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            escaped.extend(c.escape_debug());
        } else {
            escaped.push(c);
        }
    }
    Cow::Owned(escaped)
}
