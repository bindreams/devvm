//! The base build's serial log: the markers the guest writes to COM1 and the rule that lets a build go on after a
//! guest exit.
//!
//! A marker is a line that starts with a fixed prefix. Only the prefix and the numbers after it are ever read as
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

/// The markers of `log`. A line ends in `\n`, `\r` or both, and it holds a marker when it starts with a marker's prefix
/// once the terminal's control is taken out of it ([`without_control`]): text before the prefix, or a prefix in the
/// middle of a line, is no marker. `DEVVM-BASE-DONE` is a marker only when nothing follows it. Bytes that are not UTF-8
/// are read as the replacement character.
pub(super) fn read(log: &[u8]) -> Serial {
    let text = String::from_utf8_lossy(log);
    let mut serial = Serial::default();
    for line in text.split(LINE_ENDS) {
        let line = without_control(line);
        if let Some(phase) = line.strip_prefix(PROGRESS) {
            serial.progress.push(phase.trim().to_owned());
        } else if line.starts_with(LICENSE) {
            serial.license = Some(line);
        } else if let Some(why) = line.strip_prefix(FAILED) {
            serial.failed = Some(why.trim().to_owned());
        } else if line == DONE {
            serial.done = true;
        }
    }
    serial
}

/// The characters that end a line of the serial log.
const LINE_ENDS: [char; 2] = ['\n', '\r'];

const ESC: char = '\x1b';

const BEL: char = '\x07';

/// The bytes between an escape and the final byte of a sequence that has some (`ESC ( B`).
const INTERMEDIATES: std::ops::RangeInclusive<char> = ' '..='/';

/// `line` as the terminal's own control leaves it out. Escape sequences are dropped anywhere in it:
/// - `ESC [`, parameters and a final byte (CSI);
/// - `ESC ]` and text up to a BEL or `ESC \` (OSC);
/// - `ESC`, bytes `0x20` to `0x2F` and a final byte (`ESC ( B`);
/// - `ESC` and one more character.
///
/// Whitespace and control characters are dropped at its start and at its end. A sequence the line ends in the middle of
/// is dropped to the end of the line.
fn without_control(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut text = String::with_capacity(line.len());
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        at += 1;
        if c == ESC {
            at = after_sequence(&chars, at);
        } else {
            text.push(c);
        }
    }
    text.trim_start_matches(|c: char| c.is_control() || c.is_whitespace())
        .trim_end_matches(|c: char| c.is_control() || c.is_whitespace())
        .to_owned()
}

/// Where the escape sequence ends whose `ESC` is just before `at` in `chars`: the end of `chars` for one they end in
/// the middle of.
fn after_sequence(chars: &[char], at: usize) -> usize {
    // The index after the first character from `from` on that `is_final`.
    let after_first = |from: usize, is_final: &dyn Fn(char) -> bool| {
        chars[from..]
            .iter()
            .position(|&c| is_final(c))
            .map_or(chars.len(), |found| from + found + 1)
    };
    match chars.get(at) {
        None => at,
        // The final byte of a control sequence is the first one that is not a parameter or an intermediate.
        Some('[') => after_first(at + 1, &|c| ('@'..='~').contains(&c)),
        // An operating system command ends at BEL or at `ESC \`. Any other escape ends it too, and begins the next
        // sequence.
        Some(']') => match chars[at + 1..].iter().position(|&c| c == BEL || c == ESC) {
            None => chars.len(),
            Some(found) => {
                let end = at + 1 + found;
                match (chars[end], chars.get(end + 1)) {
                    (ESC, Some('\\')) => end + 2,
                    (ESC, _) => end,
                    _ => end + 1,
                }
            }
        },
        Some(c) if INTERMEDIATES.contains(c) => after_first(at + 1, &|c| !INTERMEDIATES.contains(&c)),
        Some(_) => at + 1,
    }
}

impl Serial {
    /// Adds what a later part of the same log says: its phases follow, and its license and failure replace earlier
    /// ones.
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
                .rposition(|byte| matches!(byte, b'\n' | b'\r'))
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
