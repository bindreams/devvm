//! Where a command's progress and its final error go: the screen the person reads, and the log file of the run.

use std::io::{IsTerminal, StderrLock, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

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
/// waits there. The screens stay out of its way: [`LineProgress::stderr`] writes through that lock, and
/// [`TtyProgress`] takes it around every call into indicatif, which `console::Term` does not do itself on a Windows
/// console. The lock is re-entrant, so a hook on the thread of an ending, which the ending itself failed, ends the run
/// instead of waiting for itself. A panic on another thread after the ending has taken the lock is not reported: its
/// hook waits for the exit.
///
/// A hook never stops, clears or waits for a sink: the thread that panicked may hold a sink's lock. It reads the log
/// from `active`, which is held only to clone or replace what it holds. The ending stops and joins the sinks' threads
/// before it takes the one-ending lock, and waits for nothing under it. What a Ctrl-C or a panic leaves on screen is in
/// the README's "Output".
pub(crate) struct Run {
    active: Mutex<Option<Active>>,
}

/// A command's ending or a panic hook's, written: the process exit code, with the one-ending lock still held. The
/// process must exit before it is dropped.
pub(crate) struct Ended {
    pub(crate) code: i32,
    _held: StderrLock<'static>,
}

/// The run of this process.
pub(crate) static RUN: Run = Run::new();

impl Run {
    pub(crate) const fn new() -> Run {
        Run {
            active: Mutex::new(None),
        }
    }

    fn active(&self) -> MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn begin(&self, active: Active) {
        let before = self.active().replace(active);
        debug_assert!(before.is_none(), "a command began while another had not ended");
    }

    /// The panic hook's part, on the panicking thread: waits for an ending that is under way, then writes the `log:`
    /// line and the error line, records the error and writes the status file, as an ending would. It takes no sink's
    /// lock (the thread may hold it) and stops none.
    pub(crate) fn on_panic(
        &self,
        message: String,
        colour: bool,
        status_file: Option<&Path>,
        width: ExitWidth,
        stderr: &mut dyn Write,
    ) -> Ended {
        let held = std::io::stderr().lock();
        let log = self.active().as_ref().and_then(|active| active.log.clone());
        let outcome = Err(Error::new(ErrorKind::Panic, message));
        let code = end_with(log.as_deref(), colour, status_file, outcome, width, stderr);
        Ended { code, _held: held }
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
    ) -> Ended {
        let active = self.active().clone();
        if let Some(active) = &active {
            active.stop();
        }
        let held = std::io::stderr().lock();
        let log = active.and_then(|active| active.log);
        let code = end_with(log.as_deref(), colour, status_file, outcome, width, stderr);
        // Dropped here, after the state's lock: a sink's drop may wait on a thread.
        let finished = self.active().take();
        drop(finished);
        Ended { code, _held: held }
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
