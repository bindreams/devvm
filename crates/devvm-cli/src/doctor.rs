//! `devvm doctor`: one line per check of this host.

use std::io::Write;

use devvm::exit::ExitStatus;
use devvm::{Check, Engine, Error, ErrorKind, Result};

#[cfg(test)]
#[path = "doctor_tests.rs"]
mod doctor_tests;

/// Prints every check of `engine` on stdout. Exits 125, `unavailable`, once all are printed if any failed.
pub(crate) fn doctor(engine: &Engine) -> Result<ExitStatus> {
    doctor_to(&mut std::io::stdout().lock(), engine.checks())
}

pub(crate) fn doctor_to(out: &mut dyn Write, checks: Vec<Check>) -> Result<ExitStatus> {
    let mut failed = 0;
    for check in &checks {
        failed += usize::from(check.outcome.is_err());
        writeln!(out, "{}", line(check)).map_err(|e| Error::io(ErrorKind::Io, "cannot write to stdout", e))?;
    }
    match failed {
        0 => Ok(ExitStatus::Code(0)),
        n => Err(Error::new(ErrorKind::Unavailable, format!("{n} check(s) failed"))),
    }
}

/// `pass <name>: <what was found>` or `fail <name>: <what is wrong>`.
pub(crate) fn line(check: &Check) -> String {
    match &check.outcome {
        Ok(found) => format!("pass {}: {found}", check.name),
        Err(why) => format!("fail {}: {why}", check.name),
    }
}
