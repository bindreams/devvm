//! Looking at one path below a root, by name.

use std::fs::{self, File, FileType, Metadata};
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
    /// Nothing is there ([`is_absent`]): a component is gone, is a file where a directory should be, or is a name the
    /// host cannot hold.
    Absent,
    /// None of the above, described for a message ("a FIFO").
    Other(&'static str),
}

/// Whether `e` says the path is not there on this host: gone, below a file, or a name the host cannot hold (Windows
/// reports `a|b` as an invalid name, not as absent). The single definition of "not there". A lookup that failed to
/// answer has not found the path absent, so these are not absent: a name or path too long to look up (the path may
/// exist), a share that cannot be reached (Windows), and every other error.
pub(crate) fn is_absent(e: &io::Error) -> bool {
    match e.kind() {
        io::ErrorKind::NotADirectory => true,
        io::ErrorKind::NotFound | io::ErrorKind::InvalidFilename => super::current().says_not_there(e),
        _ => false,
    }
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
