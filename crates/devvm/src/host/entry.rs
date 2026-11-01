//! Looking at one path below a root, by name.

use std::fs::{self, File, FileType};
use std::io;
use std::path::Path;

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
/// reports `a|b` as an invalid name, not as absent).
pub(crate) fn is_absent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory | io::ErrorKind::InvalidFilename
    )
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
    let kind = match fs::symlink_metadata(&path) {
        Ok(meta) => meta.file_type(),
        Err(e) if is_absent(&e) => return Ok(Entry::Absent),
        Err(e) => return Err(e),
    };
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
