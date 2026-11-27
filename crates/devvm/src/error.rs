//! The crate's error type: a stable kind, a message, and an optional source, serial log and tail.

use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};

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

/// An error goes inside another only through [`Error::naming`] and [`Error::with_step_naming`]; `format!("{inner}")`
/// into an error message is the mistake.
#[derive(Clone, Debug)]
pub struct Error {
    kind: ErrorKind,
    // The texts and the path are boxed, not `String` and `PathBuf`, so that an `Error` stays small enough to return in
    // a `Result` (`clippy::result_large_err`).
    /// The sentence, without the serial log and the tail.
    body: Box<str>,
    /// The tails, each starting with a newline; they follow the serial log.
    tail: Box<str>,
    source: Option<Source>,
    serial_log: Option<Box<Path>>,
    /// The body, the serial log and the tail as [`Error::message`] shows them. Only [`Error::render`] writes it.
    message: Box<str>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Error {
        let body = message.into().into_boxed_str();
        Error {
            kind,
            message: body.clone(),
            body,
            tail: Box::default(),
            source: None,
            serial_log: None,
        }
    }

    /// Makes the message: the body, then ` (serial log: <path>)` when there is a serial log, then the tail.
    fn render(&mut self) {
        let mut message = self.body.to_string();
        if let Some(path) = &self.serial_log {
            message.push_str(&format!(" (serial log: {})", path.display()));
        }
        message.push_str(&self.tail);
        self.message = message.into_boxed_str();
    }

    /// Sets the serial log the error is about, replacing an earlier one. The message shows its path after the body.
    pub(crate) fn with_serial_log(mut self, path: impl Into<PathBuf>) -> Error {
        self.serial_log = Some(path.into().into_boxed_path());
        self.render();
        self
    }

    pub(crate) fn serial_log(&self) -> Option<&Path> {
        self.serial_log.as_deref()
    }

    /// `text` at the end of the message, after the serial log path and the tail, as it is. Kind, source and serial log
    /// stay.
    pub(crate) fn with_appended(mut self, text: &str) -> Error {
        self.push_tail(text);
        self
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

    /// The same failure described again, as a `kind` failure of `context`: the body is `<context>: <this body>`, the
    /// old kind is not shown, and the serial log, the tail and the source stay. A different failure that names this
    /// one, its kind shown, is [`Error::naming`].
    pub(crate) fn with_context(mut self, kind: ErrorKind, context: impl fmt::Display) -> Error {
        self.body = format!("{context}: {}", self.body).into_boxed_str();
        self.kind = kind;
        self.render();
        self
    }

    /// This error with `step` after its body, before the serial log and the tail: what the person can do about it, or
    /// what came with it.
    pub(crate) fn with_step(mut self, step: impl fmt::Display) -> Error {
        self.body = format!("{}; {step}", self.body).into_boxed_str();
        self.render();
        self
    }

    /// A new error of `kind` whose body is `text(<inner as "kind: sentence">)`, for a different failure that names
    /// `inner`. It takes `inner`'s serial log, tail and source (as [`Error::with_context`] does): nothing of `inner` but
    /// its sentence is in the body.
    pub(crate) fn naming(kind: ErrorKind, inner: &Error, text: impl FnOnce(&str) -> String) -> Error {
        let mut error = Error::new(kind, text(&inner.sentence()));
        error.tail = inner.tail.clone();
        error.source = inner.source.clone();
        error.serial_log = inner.serial_log.clone();
        error.render();
        error
    }

    /// This error with a step that names `inner`, a second failure: `step(<inner as "kind: sentence">)` is added as
    /// [`Error::with_step`] adds it. This error keeps its own source. `inner`'s serial log is taken when this error has
    /// none (when both have one, this error's stays). `inner`'s tail follows this error's, each under its own label; a
    /// tail equal to this error's is not repeated.
    pub(crate) fn with_step_naming(self, inner: &Error, step: impl FnOnce(&str) -> String) -> Error {
        let mut error = self.with_step(step(&inner.sentence()));
        if error.serial_log.is_none() {
            error.serial_log = inner.serial_log.clone();
        }
        if inner.tail != error.tail {
            error.tail = format!("{}{}", error.tail, inner.tail).into_boxed_str();
        }
        error.render();
        error
    }

    /// `<kind>: <body>`: what [`Error::naming`] and [`Error::with_step_naming`] put inside another error.
    fn sentence(&self) -> String {
        format!("{}: {}", self.kind.as_str(), self.body)
    }

    /// This error once `cleanup`, which undid a step that had half happened, has run. A cleanup that failed too is
    /// named next to it with `left`, what it left behind: the first failure stays the error. `cleanup` is not devvm's
    /// [`Error`]: that one is named with [`Error::with_step_naming`].
    pub(crate) fn after_cleanup(self, left: &str, cleanup: std::result::Result<(), impl fmt::Display>) -> Error {
        match cleanup {
            Err(failed) => self.with_step(format_args!("cleaning up failed as well, so {left} remains ({failed})")),
            Ok(()) => self,
        }
    }

    /// This error with a failure that came with it named after it; the first failure stays the error. `other` is not
    /// devvm's [`Error`]: that one is named with [`Error::with_step_naming`].
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

    /// This error with the last [`TAIL_LINES`] lines of `text`, which `what` names, after its message and serial log path.
    pub(crate) fn with_tail(mut self, what: &str, text: &str) -> Error {
        self.append_tail(what, &last_lines(text));
        self
    }

    /// `tail`, which `what` names, after the message and the serial log path, as it is: the caller has cut it to the
    /// lines it wants. Nothing when it is empty.
    pub(crate) fn append_tail(&mut self, what: &str, tail: &str) {
        if !tail.is_empty() {
            self.push_tail(&format!("\nthe end of {what}:\n{tail}"));
        }
    }

    fn push_tail(&mut self, text: &str) {
        self.tail = format!("{}{text}", self.tail).into_boxed_str();
        self.render();
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

/// The text of `error` followed by the text of each error beneath it, `: ` between them, so a message carries the cause
/// (DNS, refused, TLS) and not only the top layer. A source whose text an earlier layer already says is left out.
pub(crate) fn text_with_sources(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut next = error.source();
    while let Some(source) = next {
        let said = source.to_string();
        if !text.contains(&said) {
            text.push_str(": ");
            text.push_str(&said);
        }
        next = source.source();
    }
    text
}

pub(crate) const TAIL_LINES: usize = 20;

/// The last [`TAIL_LINES`] lines of `text`, with the line ends of a Windows file taken off.
pub(crate) fn last_lines(text: &str) -> String {
    let mut lines = LastLines::default();
    for line in text.lines() {
        lines.push(line.to_owned());
    }
    lines.text()
}

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

    /// The lines joined by `\n`, with no trailing newline.
    pub(crate) fn text(&self) -> String {
        self.0.iter().map(String::as_str).collect::<Vec<_>>().join("\n")
    }
}
