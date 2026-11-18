//! Where a command's progress and its final error go: the screen the person reads, and the log file of the run.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use devvm::exit::{ExitStatus, ExitWidth, conclude, print_error_line};
use devvm::progress::{Fanout, FileLog, LineProgress, Look, Progress};
use devvm::store::layers::SystemClock;
use devvm::{Dirs, Result};

use crate::tty::TtyProgress;

#[cfg(test)]
#[path = "output_tests.rs"]
mod output_tests;

/// What `choose` reads about stderr.
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
    /// Bars and a task tree, redrawn in place.
    Tty,
    /// One concise line per event.
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

/// What the run in progress holds that its ending needs, and that a panic on any thread must reach: the log to name and
/// record in, and the terminal screen to stop before anything is written below it.
pub(crate) struct Active {
    pub(crate) log: Option<Arc<FileLog>>,
    pub(crate) tty: Option<Arc<TtyProgress>>,
}

/// The run in progress. The command line runs one command at a time per process: a run replaces this when it starts
/// and [`end`] empties it, so a later run in the same process starts from nothing.
pub(crate) struct Run(Mutex<Option<Active>>);

impl Run {
    pub(crate) const fn new() -> Run {
        Run(Mutex::new(None))
    }

    pub(crate) fn begin(&self, active: Active) {
        let before = self.0.lock().unwrap_or_else(PoisonError::into_inner).replace(active);
        debug_assert!(before.is_none(), "a command began while another had not ended");
    }

    fn take(&self) -> Option<Active> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

static RUN: Run = Run::new();

/// The screen for stderr and the log of this run in the state directory, both chosen from the process's facts, with
/// step output and debug notes when `verbose`. The run is kept for [`end`].
pub(crate) fn sinks(verbose: bool) -> Sinks {
    let look = Look {
        colour: stderr_colour(),
        verbose,
    };
    let (screen, tty): (Arc<dyn Progress>, Option<Arc<TtyProgress>>) = match choose(&Facts::of_stderr()) {
        Screen::Tty => {
            let tty = Arc::new(TtyProgress::stderr(look));
            (tty.clone(), Some(tty))
        }
        Screen::Lines => (Arc::new(LineProgress::stderr(look)), None),
    };
    let dirs = Dirs::from_env().ok();
    let sinks = compose(screen, dirs.as_ref());
    RUN.begin(Active {
        log: sinks.log.clone(),
        tty,
    });
    sinks
}

/// `screen` and the log in `dirs`. A log that cannot be made is one warning on the screen, and the command goes on
/// without it.
fn compose(screen: Arc<dyn Progress>, dirs: Option<&Dirs>) -> Sinks {
    let log = dirs.and_then(|dirs| match FileLog::create(&dirs.logs(), Arc::new(SystemClock)) {
        Ok(log) => Some(Arc::new(log)),
        Err(e) => {
            screen.warn(e.message());
            None
        }
    });
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

/// The one ending of a command: [`conclude`] it, then print the error line it names, red where stderr takes colour. A
/// failure also names the log file on the line before it and is recorded there. The error goes to the log alone, never
/// through the sinks: the screen would print it as a warning above its own line.
///
/// The terminal screen is stopped and cleared first, so that nothing it draws can erase what is written here; this is
/// also what the panic hook calls. The run is over afterwards.
pub(crate) fn end(
    status_file: Option<&Path>,
    outcome: Result<ExitStatus>,
    width: ExitWidth,
    stderr: &mut dyn Write,
) -> i32 {
    end_in(&RUN, stderr_colour(), status_file, outcome, width, stderr)
}

pub(crate) fn end_in(
    run: &Run,
    colour: bool,
    status_file: Option<&Path>,
    outcome: Result<ExitStatus>,
    width: ExitWidth,
    stderr: &mut dyn Write,
) -> i32 {
    let active = run.take();
    if let Some(tty) = active.as_ref().and_then(|active| active.tty.as_ref()) {
        tty.quiet();
    }
    let log = active.as_ref().and_then(|active| active.log.as_deref());
    end_with(log, colour, status_file, outcome, width, stderr)
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
            let _ = writeln!(stderr, "log: {}", log.path().display());
            log.failed(line);
        }
        print_error_line(stderr, line, colour);
    }
    ending.exit
}
