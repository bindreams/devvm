//! The base build's serial log: the markers the guest writes to COM1 and the rule that lets a build go on after a
//! guest exit.
//!
//! A marker is a line that starts with a fixed prefix. Only the prefix and the numbers after it are ever read as
//! meaning; the text after `DEVVM-BASE-FAILED=` is localized and only echoed.

use std::path::Path;

use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "serial_tests.rs"]
mod serial_tests;

const PROGRESS: &str = "DEVVM-PROGRESS=";
const LICENSE: &str = "DEVVM-LICENSE=";
const FAILED: &str = "DEVVM-BASE-FAILED=";
const DONE: &str = "DEVVM-BASE-DONE";

/// What one serial log says.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Serial {
    /// The phases of every `DEVVM-PROGRESS=<phase>` line, in order.
    pub progress: Vec<String>,
    /// The last whole `DEVVM-LICENSE=...` line: what the license read of [`super::super::expiry`] takes.
    pub license: Option<String>,
    /// The text after the prefix of `DEVVM-BASE-FAILED=`.
    pub failed: Option<String>,
    /// The guest wrote `DEVVM-BASE-DONE`.
    pub done: bool,
}

/// The markers of `log`. Lines end in `\n` or `\r\n`; a line that does not start with a marker's prefix is not one, and
/// bytes that are not UTF-8 are read as the replacement character.
pub(super) fn read(log: &[u8]) -> Serial {
    let text = String::from_utf8_lossy(log);
    let mut serial = Serial::default();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(phase) = line.strip_prefix(PROGRESS) {
            serial.progress.push(phase.trim().to_owned());
        } else if line.starts_with(LICENSE) {
            serial.license = Some(line.trim_end().to_owned());
        } else if let Some(why) = line.strip_prefix(FAILED) {
            serial.failed = Some(why.trim().to_owned());
        } else if line.trim_end() == DONE {
            serial.done = true;
        }
    }
    serial
}

/// The length of the part of `log` that ends in a line end: what can be read while the guest may still be writing.
pub(super) fn whole_lines(log: &[u8]) -> usize {
    log.iter().rposition(|byte| *byte == b'\n').map_or(0, |end| end + 1)
}

/// The launches of a build that ended without finishing it, and the markers they wrote. A guest reboot is a process
/// exit, so a build relaunches after each; it goes on only while the launches move forward, which replaces a count of
/// relaunches.
#[derive(Debug, Default)]
pub(super) struct Launches {
    seen: Vec<String>,
    count: usize,
}

impl Launches {
    /// Records a launch, whose serial log `log` is, that ended without the build being done or failed. The first launch
    /// is Windows Setup's first stage, which has no way to write a marker, so it needs none; every later launch must
    /// have written a marker no earlier launch wrote, or the build is looping.
    pub(super) fn note(&mut self, serial: &Serial, log: &Path) -> Result<()> {
        self.count += 1;
        let before = self.seen.len();
        for phase in &serial.progress {
            if !self.seen.contains(phase) {
                self.seen.push(phase.clone());
            }
        }
        if self.count > 1 && self.seen.len() == before {
            let last = match self.last_marker() {
                Some(phase) => format!("the last marker is `{phase}`"),
                None => "no marker was written before it".to_owned(),
            };
            let wrote = if serial.progress.is_empty() {
                "no marker".to_owned()
            } else {
                "no marker that an earlier launch did not write".to_owned()
            };
            return Err(Error::new(
                ErrorKind::Unavailable,
                format!(
                    "the guest restarted and wrote {wrote} in launch {}, so the Windows build is not moving forward \
                     ({last}); serial log: {}",
                    self.count,
                    log.display()
                ),
            ));
        }
        Ok(())
    }

    /// The newest marker any launch has written.
    pub(super) fn last_marker(&self) -> Option<&str> {
        self.seen.last().map(String::as_str)
    }
}
