//! Where a command's progress and its final error go: the screen the person reads, and the log file of the run.

use std::any::Any;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

use devvm::exit::{ExitStatus, ExitWidth, conclude, print_error_line};
use devvm::progress::{Fanout, FileLog, LineProgress, Look, Progress};
use devvm::store::layers::SystemClock;
use devvm::{Dirs, Error, ErrorKind, Result};

use crate::tty::TtyProgress;

#[cfg(test)]
#[path = "output_tests.rs"]
pub(crate) mod output_tests;

pub(crate) struct Facts {
    pub is_terminal: bool,
    /// The columns of the terminal, where the platform can tell; `None` for a pty with no console size (mintty).
    pub width: Option<u16>,
    /// `TERM=dumb`.
    pub dumb: bool,
}

impl Facts {
    fn of_stderr() -> Facts {
        Facts {
            is_terminal: std::io::stderr().is_terminal(),
            width: console::Term::stderr().size_checked().map(|(_rows, columns)| columns),
            dumb: std::env::var_os("TERM").is_some_and(|term| term == "dumb"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Screen {
    Tty,
    Lines,
}

/// Bars need a terminal whose width is known: with none, a redraw is garbled, so the person gets lines.
pub(crate) fn choose(facts: &Facts) -> Screen {
    match facts {
        Facts {
            is_terminal: true,
            width: Some(1..),
            dumb: false,
        } => Screen::Tty,
        _ => Screen::Lines,
    }
}

/// Whether stderr takes colour: a terminal, and no `NO_COLOR`.
pub(crate) fn stderr_colour() -> bool {
    console::colors_enabled_stderr()
}

// Sinks ===============================================================================================================

/// What a command reports to.
pub(crate) struct Sinks {
    pub progress: Arc<dyn Progress>,
    /// The log of this run, where one could be made.
    pub log: Option<Arc<FileLog>>,
}

/// The screen for stderr and the log of this run in the state directory, both chosen from the process's facts, with
/// step output and debug notes when `verbose`. The run is kept for [`end`].
pub(crate) fn sinks(verbose: bool) -> Sinks {
    let look = Look {
        colour: stderr_colour(),
        verbose,
    };
    let (screen, shown): (Arc<dyn Progress>, Shown) = match choose(&Facts::of_stderr()) {
        Screen::Tty => {
            let tty = Arc::new(TtyProgress::stderr(look));
            (tty.clone(), Shown::Tty(tty))
        }
        Screen::Lines => {
            let lines = Arc::new(LineProgress::stderr(look));
            (lines.clone(), Shown::Lines(lines))
        }
    };
    let sinks = compose(screen, Dirs::from_env().as_ref());
    RUN.begin(Active {
        log: sinks.log.clone(),
        screen: Some(shown),
    });
    sinks
}

/// `screen` and the log in `dirs`. A log that cannot be made, or a state directory that cannot be found, is one warning
/// on the screen, and the command goes on without it.
fn compose(screen: Arc<dyn Progress>, dirs: std::result::Result<&Dirs, &Error>) -> Sinks {
    let made = dirs
        .map_err(Error::clone)
        .and_then(|dirs| FileLog::create(&dirs.logs(), Arc::new(SystemClock)));
    let log = match made {
        Ok(log) => Some(Arc::new(log)),
        Err(e) => {
            screen.warn(e.message());
            None
        }
    };
    let mut all = vec![screen];
    if let Some(log) = &log {
        all.push(log.clone());
    }
    Sinks {
        progress: Arc::new(Fanout::new(all)),
        log,
    }
}

// Ending ==============================================================================================================

/// What the run in progress holds that its ending needs: the log to name and record in, and the screen to stop before
/// anything is written below it.
#[derive(Clone)]
pub(crate) struct Active {
    pub(crate) log: Option<Arc<FileLog>>,
    pub(crate) screen: Option<Shown>,
}

/// The screen of a run, which is stopped before the error is written.
#[derive(Clone)]
pub(crate) enum Shown {
    Tty(Arc<TtyProgress>),
    Lines(Arc<LineProgress>),
}

impl Active {
    /// Stops each sink, one at a time: the sink's lock is taken, so that a write in flight is over first, its `stopped`
    /// flag is set under it, and a terminal screen's bars are taken off. Nothing is written by a sink after this.
    fn stop(&self) {
        match &self.screen {
            Some(Shown::Tty(tty)) => tty.stop(),
            Some(Shown::Lines(lines)) => lines.stop(),
            None => {}
        }
        if let Some(log) = &self.log {
            log.stop();
        }
    }
}

/// The run in progress, and who ends it. The command line runs one command at a time per process: a run replaces
/// `active` when it starts and its ending empties it, so a later run in the same process starts from nothing.
///
/// Exactly one ending prints: the command's own ([`Run::end`]) or a panic hook's ([`Run::on_panic`]). Each writes
/// under the one-ending lock, `std::io::stderr()`'s, and keeps it until the process exits ([`Ended`]), so the other
/// waits there. Every screen writes through that lock too: indicatif's frames (`console::Term` goes through
/// `std::io::stderr()`) and [`LineProgress::stderr`]. The lock is re-entrant, so a hook on the thread of an ending,
/// which the ending itself failed, ends the run instead of waiting for itself.
///
/// A hook first notes its panic in `panicked`, then waits for the lock. An ending that holds the lock reads `panicked`
/// before it concludes and again before the process exits ([`Ended::settle`]), so a panic on another thread is the
/// outcome whenever it began before that last check; one that begins after it is the process exiting.
///
/// A hook never stops, clears or waits for a sink: the thread that panicked may hold a sink's lock. If devvm itself
/// panics, progress from other threads can still appear around the error line, and a bar redraw can overwrite it: the
/// lock keeps the error last on a pipe and on a VT terminal, but a Windows console moves the cursor and clears lines
/// through calls that do not take it, and the normal ending's clear can race a hook the same way. The exit code, the
/// status file and the log's `ERROR` record are right.
pub(crate) struct Run {
    active: Mutex<Option<Active>>,
    /// The message of the first panic of the process, set before its hook waits for the one-ending lock.
    panicked: Mutex<Option<String>>,
    /// The one-ending lock; `None` is `std::io::stderr()`'s. A test gives each run its own, so that parallel tests do
    /// not wait on one another's endings.
    lock: Option<Box<dyn OneEnding>>,
}

/// The one-ending lock's guard.
type Held = Box<dyn Any>;

/// A re-entrant lock: a thread that holds it can take it again.
pub(crate) trait OneEnding: Send + Sync {
    fn hold(&self) -> Held;
}

/// A command's ending or a panic hook's, written: the process exit code, with the one-ending lock still held. The
/// process must exit before it is dropped, through [`Ended::exit`].
pub(crate) struct Ended<'a> {
    pub(crate) code: i32,
    run: &'a Run,
    log: Option<Arc<FileLog>>,
    colour: bool,
    status_file: Option<PathBuf>,
    width: ExitWidth,
    /// Whether the outcome written is a panic.
    panicked: bool,
    _held: Held,
}

impl Ended<'_> {
    /// The last check before the exit: a panic noted since the ending began is the outcome.
    pub(crate) fn settle(mut self, stderr: &mut dyn Write) -> Self {
        if !self.panicked
            && let Some(message) = self.run.panicked()
        {
            self.code = end_with(
                self.log.as_deref(),
                self.colour,
                self.status_file.as_deref(),
                Err(Error::new(ErrorKind::Panic, message)),
                self.width,
                stderr,
            );
            self.panicked = true;
        }
        self
    }

    /// [`Ended::settle`], then exits the process with the lock still held.
    pub(crate) fn exit(self, stderr: &mut dyn Write) -> ! {
        std::process::exit(self.settle(stderr).code)
    }
}

/// The run of this process.
pub(crate) static RUN: Run = Run::new();

impl Run {
    pub(crate) const fn new() -> Run {
        Run {
            active: Mutex::new(None),
            panicked: Mutex::new(None),
            lock: None,
        }
    }

    fn active(&self) -> MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The run, if it can be read now. A hook gets none rather than wait: a thread that panicked while it held `active`
    /// would be waited for by its own hook.
    fn try_active(&self) -> Option<Active> {
        match self.active.try_lock() {
            Ok(active) => active.clone(),
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().clone(),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// The message of the first panic noted, if any.
    pub(crate) fn panicked(&self) -> Option<String> {
        self.panicked.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn hold(&self) -> Held {
        match &self.lock {
            Some(lock) => lock.hold(),
            None => Box::new(std::io::stderr().lock()),
        }
    }

    pub(crate) fn begin(&self, active: Active) {
        let before = self.active().replace(active);
        debug_assert!(before.is_none(), "a command began while another had not ended");
    }

    /// The panic hook's part, on the panicking thread: notes the panic, waits for an ending that is under way, then
    /// writes the `log:` line and the error line, records the error and writes the status file, as an ending would. It
    /// takes no sink's lock (the thread may hold it) and stops none.
    pub(crate) fn on_panic(
        &self,
        message: String,
        colour: bool,
        status_file: Option<&Path>,
        width: ExitWidth,
        stderr: &mut dyn Write,
    ) -> Ended<'_> {
        self.panicked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(|| message.clone());
        let outcome = Err(Error::new(ErrorKind::Panic, message));
        self.ending(false, colour, status_file, outcome, width, stderr)
    }

    /// The one ending of a command: stops the sinks, takes the one-ending lock, [`conclude`]s, and prints the error
    /// line it names, red where `colour` is set. A failure also names the log file on the line before it and is
    /// recorded there. The error goes to the log alone, never through the sinks: the screen would print it as a
    /// warning above its own line.
    ///
    /// The run stays in `self` until the ending is written, so that a panic on this thread finds the log.
    pub(crate) fn end(
        &self,
        colour: bool,
        status_file: Option<&Path>,
        outcome: Result<ExitStatus>,
        width: ExitWidth,
        stderr: &mut dyn Write,
    ) -> Ended<'_> {
        self.ending(true, colour, status_file, outcome, width, stderr)
    }

    /// The tail of both endings: the sinks are stopped first when `stop`, then the lock is taken, and a panic noted by
    /// now is the outcome.
    fn ending(
        &self,
        stop: bool,
        colour: bool,
        status_file: Option<&Path>,
        outcome: Result<ExitStatus>,
        width: ExitWidth,
        stderr: &mut dyn Write,
    ) -> Ended<'_> {
        let active = if stop {
            let active = self.active().clone();
            if let Some(active) = &active {
                active.stop();
            }
            active
        } else {
            self.try_active()
        };
        let held = self.hold();
        let (outcome, panicked) = match self.panicked() {
            Some(message) => (Err(Error::new(ErrorKind::Panic, message)), true),
            None => (outcome, false),
        };
        let log = active.and_then(|active| active.log);
        let code = end_with(log.as_deref(), colour, status_file, outcome, width, stderr);
        if stop {
            self.active().take();
        }
        Ended {
            code,
            run: self,
            log,
            colour,
            status_file: status_file.map(Path::to_owned),
            width,
            panicked,
            _held: held,
        }
    }
}

fn end_with(
    log: Option<&FileLog>,
    colour: bool,
    status_file: Option<&Path>,
    outcome: Result<ExitStatus>,
    width: ExitWidth,
    stderr: &mut dyn Write,
) -> i32 {
    let ending = conclude(status_file, &outcome, width);
    if let Some(line) = &ending.error_line {
        // The line printed is the line recorded: the status file can turn a success into a failure.
        if let Some(log) = log {
            log.failed(line);
            let _ = writeln!(stderr, "log: {}", log.describe());
        }
        print_error_line(stderr, line, colour);
    }
    ending.exit
}
