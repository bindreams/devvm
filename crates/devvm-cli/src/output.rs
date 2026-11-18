//! Where a command's progress and its final error go: the screen the person reads, and the log file of the run.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use devvm::exit::{ExitStatus, ExitWidth, finish};
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

static VERBOSE: AtomicBool = AtomicBool::new(false);
static LOG: OnceLock<Arc<FileLog>> = OnceLock::new();

pub(crate) fn set_verbose(verbose: bool) {
    VERBOSE.store(verbose, Ordering::Relaxed);
}

/// The screen for stderr and the log of this run in the state directory, both chosen from the process's facts. The log
/// is also kept for [`end`].
pub(crate) fn sinks() -> Sinks {
    let look = Look {
        colour: stderr_colour(),
        verbose: VERBOSE.load(Ordering::Relaxed),
    };
    let screen: Arc<dyn Progress> = match choose(&Facts::of_stderr()) {
        Screen::Tty => Arc::new(TtyProgress::stderr(look)),
        Screen::Lines => Arc::new(LineProgress::stderr(look)),
    };
    let dirs = Dirs::from_env().ok();
    let sinks = compose(screen, dirs.as_ref());
    if let Some(log) = &sinks.log {
        let _ = LOG.set(log.clone());
    }
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

/// Ends a command as [`finish`] does, with the error line red where stderr takes colour. A failure also names the log
/// file on the line before it and is recorded there. The error goes to the log alone, never through the sinks: the
/// screen would print it as a warning above its own line.
pub(crate) fn end(
    status_file: Option<&Path>,
    outcome: Result<ExitStatus>,
    width: ExitWidth,
    stderr: &mut dyn Write,
) -> i32 {
    end_with(
        LOG.get().map(|log| &**log),
        stderr_colour(),
        status_file,
        outcome,
        width,
        stderr,
    )
}

fn end_with(
    log: Option<&FileLog>,
    colour: bool,
    status_file: Option<&Path>,
    outcome: Result<ExitStatus>,
    width: ExitWidth,
    stderr: &mut dyn Write,
) -> i32 {
    if let (Err(error), Some(log)) = (&outcome, log) {
        let _ = writeln!(stderr, "log: {}", log.path().display());
        log.failed(error);
    }
    finish(status_file, outcome, width, stderr, colour)
}
