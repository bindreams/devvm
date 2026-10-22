//! The crate's error type: a stable kind, a message, and an optional source.

use std::fmt;

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
    DetachRefused,
    NoExitStatus,
    ConnectionLost,
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
            ErrorKind::DetachRefused => "detach-refused",
            ErrorKind::NoExitStatus => "no-exit-status",
            ErrorKind::ConnectionLost => "connection-lost",
            ErrorKind::Unavailable => "unavailable",
            ErrorKind::NotSupported => "not-supported",
            ErrorKind::Panic => "panic",
            ErrorKind::Internal => "internal",
        }
    }
}

type Source = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A devvm failure.
#[derive(Debug)]
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
        self.source = Some(Box::new(source));
        self
    }

    /// An I/O failure; the message is `<context>: <io error>`.
    pub fn io(kind: ErrorKind, context: impl Into<String>, source: std::io::Error) -> Error {
        Error::new(kind, format!("{}: {source}", context.into())).with_source(source)
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
