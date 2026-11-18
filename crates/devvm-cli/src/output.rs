//! Where a command's progress and its final error go: the screen the person reads, and the log file of the run.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread::{self, ThreadId};

use devvm::exit::{ExitStatus, ExitWidth, conclude, print_error_line};
use devvm::progress::{Fanout, FileLog, LineProgress, Look, Progress};
use devvm::store::layers::SystemClock;
use devvm::{Dirs, Error, ErrorKind, Result};

use crate::tty::TtyProgress;

#[cfg(test)]
#[path = "output_tests.rs"]
mod output_tests;

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

impl Shown {
    /// Stops drawing and takes what was drawn off the screen.
    fn quiet(&self) {
        match self {
            Shown::Tty(tty) => tty.quiet(),
            Shown::Lines(lines) => lines.stop(),
        }
    }

    /// Stops drawing, for a panic hook that may be on a thread that holds the screen's lock.
    fn panicked(&self) {
        match self {
            Shown::Tty(tty) => tty.panicked(),
            Shown::Lines(lines) => lines.stop(),
        }
    }
}

/// The run in progress, and who ends it. The command line runs one command at a time per process: a run replaces
/// `active` when it starts and its ending empties it, so a later run in the same process starts from nothing.
///
/// Exactly one ending prints. Either the command's own [`Ending`] does, or a panic hook does when no ending is under
/// way ([`Run::on_panic`]). The state mutex decides which: a hook that ends the run keeps it locked until the process
/// exits, so another panic or an ending waits there. A hook on another thread that finds an ending under way stores its
/// message and returns, so that its thread unwinds and lets go of what the ending waits for; the ending reports it. A
/// hook on the ending's own thread ends the run: the ending itself failed.
pub(crate) struct Run {
    active: Mutex<Option<Active>>,
    state: Mutex<State>,
}

pub(crate) enum State {
    Idle,
    /// A command's [`Ending`] is under way.
    Ending {
        /// The thread running it.
        thread: ThreadId,
        /// The first panic another thread's hook stored.
        panic: Option<String>,
        /// The outcome is decided: a panic after this is dropped, since the process is exiting.
        sealed: bool,
    },
    /// A panic hook is the ending.
    Panicking,
}

/// What [`Run::on_panic`] did.
pub(crate) enum Panicked<'a> {
    /// This hook was the ending and printed it. The process must exit with `code` while `_held` is kept.
    Ended { code: i32, _held: MutexGuard<'a, State> },
    /// A command's ending is under way: it reports the panic.
    Deferred,
}

/// The run of this process.
pub(crate) static RUN: Run = Run::new();

impl Run {
    pub(crate) const fn new() -> Run {
        Run {
            active: Mutex::new(None),
            state: Mutex::new(State::Idle),
        }
    }

    pub(crate) fn begin(&self, active: Active) {
        let before = self
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(active);
        debug_assert!(before.is_none(), "a command began while another had not ended");
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The command's ending starts: a panic from now on is stored for [`Ending::end`] instead of ending the process.
    /// Waits while a panic hook is the ending, which is until the process exits.
    pub(crate) fn begin_ending(&self) -> Ending<'_> {
        *self.state() = State::Ending {
            thread: thread::current().id(),
            panic: None,
            sealed: false,
        };
        Ending { run: self }
    }

    /// The panic hook's part, on the panicking thread. Never takes the screen's lock (the thread may hold it) and never
    /// drops the screen or the log. A bar of the last frame can stay above the error line; the error line is the last.
    pub(crate) fn on_panic(
        &self,
        message: String,
        colour: bool,
        status_file: Option<&Path>,
        width: ExitWidth,
        stderr: &mut dyn Write,
    ) -> Panicked<'_> {
        // First: a thread that panicked inside a draw is no longer in it, whatever the hook does next.
        crate::tty::release_draw();
        let mut state = self.state();
        // A panic on the ending's own thread cannot be left to it: the ending is what failed.
        if let State::Ending { thread, panic, sealed } = &mut *state
            && *thread != thread::current().id()
        {
            if !*sealed && panic.is_none() {
                *panic = Some(message);
            }
            return Panicked::Deferred;
        }
        *state = State::Panicking;
        // A lock that is busy is held by a thread in the middle of `begin`: there is no screen to tell.
        let active = match self.active.try_lock() {
            Ok(active) => Some(active),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        let active = active.as_deref().and_then(Option::as_ref);
        if let Some(screen) = active.and_then(|active| active.screen.as_ref()) {
            screen.panicked();
        }
        let log = active.and_then(|active| active.log.as_deref());
        let code = end_with(
            log,
            colour,
            status_file,
            Err(Error::new(ErrorKind::Panic, message)),
            width,
            stderr,
        );
        Panicked::Ended { code, _held: state }
    }
}

/// A command's ending, from the end of its action until the process exits.
pub(crate) struct Ending<'a> {
    run: &'a Run,
}

impl Drop for Ending<'_> {
    fn drop(&mut self) {
        *self.run.state() = State::Idle;
    }
}

impl Ending<'_> {
    /// The one ending of a command: [`conclude`] it, then print the error line it names, red where `colour` is set. A
    /// failure also names the log file on the line before it and is recorded there. The error goes to the log alone,
    /// never through the sinks: the screen would print it as a warning above its own line.
    ///
    /// The terminal screen is stopped and cleared first, so that nothing it draws can erase what is written here. A
    /// panic a hook stored while this waited for the screen is the outcome, whatever `outcome` was.
    pub(crate) fn end(
        &self,
        colour: bool,
        status_file: Option<&Path>,
        outcome: Result<ExitStatus>,
        width: ExitWidth,
        stderr: &mut dyn Write,
    ) -> i32 {
        // The run stays in `RUN` until the ending is written: a panic on this thread finds it, and its hook ends the run
        // with the screen and the log.
        let active = self.run.active.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(screen) = active.as_ref().and_then(|active| active.screen.as_ref()) {
            screen.quiet();
        }
        let outcome = match self.seal() {
            Some(message) => Err(Error::new(ErrorKind::Panic, message)),
            None => outcome,
        };
        let log = active.as_ref().and_then(|active| active.log.as_deref());
        let code = end_with(log, colour, status_file, outcome, width, stderr);
        self.run.active.lock().unwrap_or_else(PoisonError::into_inner).take();
        code
    }

    /// [`Ending::end`] for a command that succeeded and needs no status file: `0`, unless a hook stored a panic.
    pub(crate) fn end_clean(
        &self,
        colour: bool,
        status_file: Option<&Path>,
        width: ExitWidth,
        stderr: &mut dyn Write,
    ) -> i32 {
        match self.seal() {
            None => 0,
            Some(message) => self.end(
                colour,
                status_file,
                Err(Error::new(ErrorKind::Panic, message)),
                width,
                stderr,
            ),
        }
    }

    /// Decides the outcome: the panic a hook stored, and no later one is kept.
    fn seal(&self) -> Option<String> {
        match &mut *self.run.state() {
            State::Ending { panic, sealed, .. } => {
                *sealed = true;
                panic.take()
            }
            State::Idle | State::Panicking => None,
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
