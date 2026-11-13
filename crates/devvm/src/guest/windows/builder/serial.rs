//! The base build's serial log: the markers the guest writes to COM1 and the rule that lets a build go on after a
//! guest exit.
//!
//! A marker is a line that holds a fixed prefix. Only the prefix and the numbers after it are ever read as
//! meaning; the text after `DEVVM-BASE-FAILED=` is localized and only echoed.

use std::path::{Path, PathBuf};

use crate::host::{is_absent, read_from};
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

/// The markers of `log`. Lines end in `\n` or `\r\n`; a line holds a marker where its first marker prefix is, whatever
/// precedes it (a serial terminal may write escape bytes first), and the text from the prefix on is the marker. Bytes
/// that are not UTF-8 are read as the replacement character.
pub(super) fn read(log: &[u8]) -> Serial {
    let text = String::from_utf8_lossy(log);
    let mut serial = Serial::default();
    for line in text.split('\n') {
        match marker_in(line.trim_end_matches('\r')) {
            Some(Marker::Progress(phase)) => serial.progress.push(phase.trim().to_owned()),
            Some(Marker::License(read)) => serial.license = Some(read.trim_end().to_owned()),
            Some(Marker::Failed(why)) => serial.failed = Some(why.trim().to_owned()),
            Some(Marker::Done) => serial.done = true,
            None => {}
        }
    }
    serial
}

/// What a line's marker says, from its prefix on.
enum Marker<'a> {
    Progress(&'a str),
    License(&'a str),
    Failed(&'a str),
    Done,
}

/// The marker whose prefix comes first in `line`. `DEVVM-BASE-DONE` is a marker only when nothing follows it.
fn marker_in(line: &str) -> Option<Marker<'_>> {
    let done = line
        .find(DONE)
        .filter(|at| line[at + DONE.len()..].trim_end().is_empty());
    [
        line.find(PROGRESS)
            .map(|at| (at, Marker::Progress(&line[at + PROGRESS.len()..]))),
        line.find(LICENSE).map(|at| (at, Marker::License(&line[at..]))),
        line.find(FAILED)
            .map(|at| (at, Marker::Failed(&line[at + FAILED.len()..]))),
        done.map(|at| (at, Marker::Done)),
    ]
    .into_iter()
    .flatten()
    .min_by_key(|(at, _)| *at)
    .map(|(_, marker)| marker)
}

impl Serial {
    /// Adds what a later part of the same log says: its phases follow, and its license and failure replace earlier ones.
    fn append(&mut self, later: &Serial) {
        self.progress.extend(later.progress.iter().cloned());
        self.license = later.license.clone().or(self.license.take());
        self.failed = later.failed.clone().or(self.failed.take());
        self.done |= later.done;
    }
}

/// A launch's serial log, read as the guest writes it: each poll reads only the bytes past the ones it has, so a long
/// build does not read its log again for every poll, and a line the guest is still writing is kept in memory until its
/// line end comes.
pub(super) struct LogReader {
    path: PathBuf,
    /// How many bytes of the file are taken in, the unfinished last line included.
    read: u64,
    /// The bytes after the last line end seen: a line the guest is still writing.
    unfinished: Vec<u8>,
    whole: Serial,
}

impl LogReader {
    pub(super) fn new(path: &Path) -> LogReader {
        LogReader {
            path: path.to_owned(),
            read: 0,
            unfinished: Vec::new(),
            whole: Serial::default(),
        }
    }

    /// Takes in what the guest wrote since the last call and returns the markers of that part alone. A line the guest
    /// is still writing waits for its line end, unless the log is `complete` (the guest is gone): then a last line
    /// without one is read too. A log QEMU has not made yet is empty. An error does not name the log's path: the
    /// build's directory is gone when it is read.
    pub(super) fn poll(&mut self, complete: bool) -> Result<Serial> {
        let new = self.read_new()?;
        self.read += new.len() as u64;
        // `unfinished` holds no line end, so the last one is in the new bytes if it is anywhere.
        let before = self.unfinished.len();
        self.unfinished.extend_from_slice(&new);
        let used = if complete {
            self.unfinished.len()
        } else {
            new.iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |end| before + end + 1)
        };
        let part = read(&self.unfinished[..used]);
        self.unfinished.drain(..used);
        self.whole.append(&part);
        Ok(part)
    }

    /// Every marker taken in so far.
    pub(super) fn whole(&self) -> &Serial {
        &self.whole
    }

    fn read_new(&self) -> Result<Vec<u8>> {
        match read_from(&self.path, self.read) {
            Err(e) if is_absent(&e) => Ok(Vec::new()),
            other => other.map_err(|e| Error::io(ErrorKind::Io, "cannot read the serial log", e)),
        }
    }
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
    /// Records a launch that ended without the build being done or failed. The first launch is Windows Setup's first
    /// stage, which has no way to write a marker, so it needs none; every later launch must have written a marker no
    /// earlier launch wrote, or the build is looping.
    pub(super) fn note(&mut self, serial: &Serial) -> Result<()> {
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
                     ({last})",
                    self.count
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
