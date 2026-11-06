//! Looking at one path below a root, by name.

use std::fs::{self, File, FileType, Metadata};
use std::io;
use std::path::{Path, PathBuf};

#[cfg(test)]
#[path = "entry_tests.rs"]
mod entry_tests;

/// What [`Host::open_entry`](super::Host::open_entry) found.
#[derive(Debug)]
pub(crate) enum Entry {
    /// A regular file, open for reading.
    File(File),
    /// A symbolic link or, on Windows, a junction. It was not followed.
    Symlink,
    Directory,
    /// Nothing is there: a component is gone, is a file where a directory should be, or is a name the host cannot hold.
    Absent,
    /// None of the above, described for a message ("a FIFO").
    Other(&'static str),
}

/// Whether `e` says the path is not there on this host: gone, below a file, or a name the host cannot hold (Windows
/// reports `a|b` as an invalid name, not as absent). The single definition of "not there": a lookup that fails for
/// another reason has not found the path absent.
pub(crate) fn is_absent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory | io::ErrorKind::InvalidFilename
    )
}

/// The error for a link met where a file is wanted.
pub(crate) fn is_a_link(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{} is a link", path.display()))
}

/// What is at `path`, not following a link: `None` when it is not there ([`is_absent`]), the error for any other
/// failure.
pub(crate) fn symlink_metadata_if_present(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if is_absent(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `path`, or its nearest ancestor that is there when it is not; the error of a look that fails for any other reason, or
/// `NotFound` when no ancestor is there (a relative path with nothing of it in the working directory).
pub(crate) fn nearest_existing(path: &Path) -> io::Result<&Path> {
    for candidate in path.ancestors() {
        if symlink_metadata_if_present(candidate)
            .map_err(|e| look_failed(candidate, e))?
            .is_some()
        {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no part of {} exists", path.display()),
    ))
}

/// [`nearest_existing`], when what it finds is a directory (a link to one included): where a directory below `path`
/// would be made. A file there, or a link to one, is `NotADirectory` naming it: nothing can be made below it.
pub(crate) fn nearest_existing_dir(path: &Path) -> io::Result<&Path> {
    let existing = nearest_existing(path)?;
    if fs::metadata(existing).map_err(|e| look_failed(existing, e))?.is_dir() {
        return Ok(existing);
    }
    Err(io::Error::new(
        io::ErrorKind::NotADirectory,
        format!("{} is not a directory", existing.display()),
    ))
}

/// The failure `e` of a look at `path`, said as "cannot look at <path>: <e>" with its kind kept: the look failed, and
/// that says nothing about what is at `path`.
pub(crate) fn look_failed(path: &Path, e: io::Error) -> io::Error {
    io::Error::new(
        e.kind(),
        LookFailed {
            path: path.to_owned(),
            source: e,
        },
    )
}

#[derive(Debug)]
struct LookFailed {
    path: PathBuf,
    source: io::Error,
}

impl std::fmt::Display for LookFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot look at {}: {}", self.path.display(), self.source)
    }
}

impl std::error::Error for LookFailed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// [`Host::open_entry`](super::Host::open_entry) for a host that supplies how a regular file is opened and how any
/// other kind of file is named in a message.
pub(super) fn open_entry(
    root: &Path,
    rel: &str,
    open_file: fn(&Path) -> io::Result<File>,
    describe: fn(FileType) -> &'static str,
) -> io::Result<Entry> {
    let path = root.join(rel);
    let Some(meta) = symlink_metadata_if_present(&path)? else {
        return Ok(Entry::Absent);
    };
    let kind = meta.file_type();
    if kind.is_symlink() {
        return Ok(Entry::Symlink);
    }
    if kind.is_dir() {
        return Ok(Entry::Directory);
    }
    if !kind.is_file() {
        return Ok(Entry::Other(describe(kind)));
    }
    let file = open_file(&path)?;
    // What was opened is what is read, whatever the name has become since it was looked at.
    let kind = file.metadata()?.file_type();
    if kind.is_file() {
        Ok(Entry::File(file))
    } else {
        Ok(Entry::Other(describe(kind)))
    }
}
