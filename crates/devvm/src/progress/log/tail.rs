//! The end of a log file, put on an error.

use std::io::{self, BufRead, BufReader};
use std::path::Path;

use crate::error::{Error, LastLines};
use crate::host::is_absent;

#[cfg(test)]
#[path = "tail_tests.rs"]
mod tail_tests;

impl Error {
    /// This error with the last [`TAIL_LINES`](crate::error::TAIL_LINES) lines of the file at `path`, which `what`
    /// names, after its message, as [`Error::with_tail`] does. The file is read when the error is made: it may be
    /// gone, with its directory, by the time the error is read. A missing file ([`is_absent`]) or an empty one adds
    /// nothing; a file that cannot be read adds `(cannot be read: <error>)`. The file is streamed, and a line longer
    /// than [`TAIL_LINE_BYTES`] shows `...` and its end.
    pub(crate) fn with_log_tail(mut self, what: &str, path: &Path) -> Error {
        match tail_of_file(path) {
            Ok(tail) => self.append_tail(what, &tail),
            Err(e) if is_absent(&e) => {}
            Err(e) => self.append_tail(what, &format!("(cannot be read: {e})")),
        }
        self
    }
}

/// A line of a log is kept up to this many bytes, from its end. Only [`tail_of`] caps a line: it bounds the memory of a
/// file read in a stream, and text given to `last_lines` or `Tail::push` is already in memory.
const TAIL_LINE_BYTES: usize = 8 << 10;

/// `line` as text. It is the end of a longer one if `cut`: without the bytes of a character whose start was cut off,
/// and with `...` before it.
fn marked(line: &[u8], cut: bool) -> String {
    if !cut {
        return String::from_utf8_lossy(line).into_owned();
    }
    let whole = line.iter().position(|&byte| byte & 0xC0 != 0x80).unwrap_or(line.len());
    format!("...{}", String::from_utf8_lossy(&line[whole..]))
}

/// The last [`TAIL_LINES`](crate::error::TAIL_LINES) lines of the file at `path`, read in one pass.
fn tail_of_file(path: &Path) -> io::Result<String> {
    tail_of(BufReader::new(std::fs::File::open(path)?))
}

/// The last [`TAIL_LINES`](crate::error::TAIL_LINES) lines of `file`, `\r\n` read as `\n`, each capped at
/// [`TAIL_LINE_BYTES`] and decoded lossily.
fn tail_of(mut file: impl BufRead) -> io::Result<String> {
    let mut lines = LastLines::default();
    let mut line: Vec<u8> = Vec::new();
    // Whether the start of `line` was dropped to keep it under the cap.
    let mut cut = false;
    // Whether bytes of a line that has not ended were seen.
    let mut open = false;
    loop {
        let chunk = match file.fill_buf() {
            Ok(chunk) => chunk,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if chunk.is_empty() {
            break;
        }
        let (part, ended) = match chunk.iter().position(|&byte| byte == b'\n') {
            Some(at) => (&chunk[..at], Some(at + 1)),
            None => (chunk, None),
        };
        open = true;
        line.extend_from_slice(part);
        let excess = line.len().saturating_sub(TAIL_LINE_BYTES);
        line.drain(..excess);
        cut |= excess > 0;
        let used = ended.unwrap_or(chunk.len());
        file.consume(used);
        if ended.is_some() {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            lines.push(marked(&std::mem::take(&mut line), cut));
            cut = false;
            open = false;
        }
    }
    if open {
        lines.push(marked(&line, cut));
    }
    Ok(lines.text())
}
