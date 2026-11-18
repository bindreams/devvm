//! `devvm doctor`: one line per check of this host.

use std::io::Write;
use std::sync::Arc;

use devvm::exit::ExitStatus;
use devvm::progress::{Colour, Progress, escape_controls, paint};
use devvm::store::fetch::Transport;
use devvm::{Check, Engine, Error, ErrorKind, Result};

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod doctor_tests;

/// Prints every check of this host on stdout. Exits 125, `unavailable`, once all are printed if any failed. A broken
/// `config.yaml` is a failed line, not an early exit.
pub(crate) fn doctor(transport: Arc<dyn Transport>, progress: Arc<dyn Progress>) -> Result<ExitStatus> {
    let checks = Engine::doctor_checks(progress.clone(), transport)?;
    // The log keeps what the checks found; stdout has its own copy.
    for check in &checks {
        progress.debug(&line(check, false));
    }
    doctor_to(&mut std::io::stdout().lock(), checks, console::colors_enabled())
}

/// Prints `checks` to `out`, with `pass` and `fail` coloured when `colour`.
pub(crate) fn doctor_to(out: &mut dyn Write, checks: Vec<Check>, colour: bool) -> Result<ExitStatus> {
    let mut failed = 0;
    for check in &checks {
        failed += usize::from(check.outcome.is_err());
        writeln!(out, "{}", line(check, colour)).map_err(crate::stdout_error)?;
    }
    match failed {
        0 => Ok(ExitStatus::Code(0)),
        n => Err(Error::new(ErrorKind::Unavailable, format!("{n} check(s) failed"))),
    }
}

/// `pass <name>: <what was found>` or `fail <name>: <what is wrong>`, on one line: a line break or any other control
/// character in the text is written as its escape (`\n`). With `colour`, `pass` is green and `fail` red.
pub(crate) fn line(check: &Check, colour: bool) -> String {
    let (word, shade, text) = match &check.outcome {
        Ok(found) => ("pass", Colour::Green, found),
        Err(why) => ("fail", Colour::Red, why),
    };
    let word = paint(colour, shade, word);
    format!("{word} {}: {}", check.name, escape_controls(text))
}
