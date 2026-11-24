//! The crate's error type: a stable kind, a message, and an optional source.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use crate::host::is_absent;

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;

pub type Result<T> = std::result::Result<T, Error>;

/// What went wrong. The `as_str` names are part of the status-file contract.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Usage,
    NotFound,
    AlreadyExists,
    InvalidName,
    Recipe,
    Config,
    Io,
    Download,
    DiskFull,
    Checksum,
    ExpiredBase,
    Store,
    StatusFile,
    NoExitStatus,
    ConnectionLost,
    /// The guest presented a host key other than the pinned one.
    HostKey,
    /// The VM's process record cannot be checked on this host; `InstanceDir::find_machine` adds the step by hand.
    ProcessRecord,
    Unavailable,
    NotSupported,
    Panic,
    Internal,
}

impl ErrorKind {
    /// The kebab-case wire name, as printed after `devvm-error: ` and stored in the status file.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Usage => "usage",
            ErrorKind::NotFound => "not-found",
            ErrorKind::AlreadyExists => "already-exists",
            ErrorKind::InvalidName => "invalid-name",
            ErrorKind::Recipe => "recipe",
            ErrorKind::Config => "config",
            ErrorKind::Io => "io",
            ErrorKind::Download => "download",
            ErrorKind::DiskFull => "disk-full",
            ErrorKind::Checksum => "checksum",
            ErrorKind::ExpiredBase => "expired-base",
            ErrorKind::Store => "store",
            ErrorKind::StatusFile => "status-file",
            ErrorKind::NoExitStatus => "no-exit-status",
            ErrorKind::ConnectionLost => "connection-lost",
            ErrorKind::HostKey => "host-key",
            ErrorKind::ProcessRecord => "process-record",
            ErrorKind::Unavailable => "unavailable",
            ErrorKind::NotSupported => "not-supported",
            ErrorKind::Panic => "panic",
            ErrorKind::Internal => "internal",
        }
    }
}

/// Shared, so that an `Error` can be cloned with its source.
type Source = std::sync::Arc<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Clone, Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    source: Option<Source>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Error {
        Error {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// Attaches the underlying error, reachable through [`std::error::Error::source`].
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Error {
        self.source = Some(std::sync::Arc::new(source));
        self
    }

    /// An I/O failure; the message is `<context>: <io error>`.
    pub fn io(kind: ErrorKind, context: impl Into<String>, source: std::io::Error) -> Error {
        Error::new(kind, format!("{}: {source}", context.into())).with_source(source)
    }

    /// This error as a `kind` failure of `context`: the message is `<context>: <this message>` and the source is this
    /// error's source, not this error.
    pub(crate) fn with_context(self, kind: ErrorKind, context: impl fmt::Display) -> Error {
        Error {
            kind,
            message: format!("{context}: {}", self.message),
            source: self.source,
        }
    }

    /// This error with `step` after its message: what the person can do about it, or what came with it.
    pub(crate) fn with_step(mut self, step: impl fmt::Display) -> Error {
        self.message = format!("{}; {step}", self.message);
        self
    }

    /// This error once `cleanup`, which undid a step that had half happened, has run. A cleanup that failed too is
    /// named next to it with `left`, what it left behind: the first failure stays the error.
    pub(crate) fn after_cleanup(self, left: &str, cleanup: std::result::Result<(), impl fmt::Display>) -> Error {
        match cleanup {
            Err(failed) => self.with_step(format_args!("cleaning up failed as well, so {left} remains ({failed})")),
            Ok(()) => self,
        }
    }

    /// This error with a failure that came with it named after it; the first failure stays the error.
    pub(crate) fn together_with(self, other: impl fmt::Display) -> Error {
        self.with_step(format_args!("{other} as well"))
    }

    /// A failed write to `path`: `disk-full` when the device or quota is full, otherwise `io`. `hint` says what to
    /// do about a full disk and ends the `disk-full` message.
    pub(crate) fn write(path: &Path, source: std::io::Error, hint: &str) -> Error {
        match source.kind() {
            std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => Error::new(
                ErrorKind::DiskFull,
                format!("no space left to write {}: {source}; {hint}", path.display()),
            )
            .with_source(source),
            _ => Error::io(ErrorKind::Io, format!("cannot write {}", path.display()), source),
        }
    }

    /// This error with the last [`TAIL_LINES`] lines of `text`, which `what` names, after its message.
    pub(crate) fn with_tail(mut self, what: &str, text: &str) -> Error {
        self.append_tail(what, &last_lines(text));
        self
    }

    fn append_tail(&mut self, what: &str, tail: &str) {
        if !tail.is_empty() {
            self.message = format!("{}\nthe end of {what}:\n{tail}", self.message);
        }
    }

    /// This error with the last [`TAIL_LINES`] lines of the file at `path`, which `what` names, after its message, as
    /// [`Error::with_tail`] does: the file may be gone, with its directory, when the error is read. A missing file
    /// ([`is_absent`]) or an empty one adds nothing; a file that cannot be read adds `(cannot be read: <error>)`. The
    /// file is streamed, and a line longer than [`TAIL_LINE_BYTES`] shows `...` and its end.
    pub(crate) fn with_log_tail(mut self, what: &str, path: &Path) -> Error {
        match tail_of_file(path) {
            Ok(tail) => self.append_tail(what, &tail),
            Err(e) if is_absent(&e) => {}
            Err(e) => self.append_tail(what, &format!("(cannot be read: {e})")),
        }
        self
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

pub(crate) const TAIL_LINES: usize = 20;

/// The last [`TAIL_LINES`] lines of `text`, with the line ends of a Windows file taken off.
pub(crate) fn last_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n")
}

/// A line of a log is kept up to this many bytes, from its end.
const TAIL_LINE_BYTES: usize = 8 << 10;

/// The last [`TAIL_LINES`] lines pushed.
#[derive(Default)]
pub(crate) struct LastLines(VecDeque<String>);

impl LastLines {
    pub(crate) fn push(&mut self, line: String) {
        if self.0.len() == TAIL_LINES {
            self.0.pop_front();
        }
        self.0.push_back(line);
    }

    /// The lines, one to a line.
    pub(crate) fn text(&self) -> String {
        self.0.iter().map(String::as_str).collect::<Vec<_>>().join("\n")
    }
}

/// `line` as text. It is the end of a longer one if `cut`: without the bytes of a character whose start was cut off,
/// and with `...` before it.
fn marked(line: &[u8], cut: bool) -> String {
    if !cut {
        return String::from_utf8_lossy(line).into_owned();
    }
    let whole = line.iter().position(|&byte| byte & 0xC0 != 0x80).unwrap_or(line.len());
    format!("...{}", String::from_utf8_lossy(&line[whole..]))
}

/// The last [`TAIL_LINES`] lines of the file at `path`, read in one pass without holding the file.
fn tail_of_file(path: &Path) -> io::Result<String> {
    tail_of(BufReader::new(std::fs::File::open(path)?))
}

/// The last [`TAIL_LINES`] lines `file` has.
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
