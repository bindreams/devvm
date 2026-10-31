//! Looking at one path below a root without following a link on the way.

use std::fs::File;
use std::io;

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
    /// A directory above the entry is a link; this is its path below the root, `/`-separated.
    BelowSymlink(String),
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

/// The names in `rel`, which is `/`-separated and below some root: no empty name, no `.` or `..`, no NUL.
pub(super) fn components(rel: &str) -> io::Result<Vec<&str>> {
    let names: Vec<&str> = rel.split('/').collect();
    if names.iter().any(|name| matches!(*name, "" | "." | "..")) || rel.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("`{}` is not a path below a directory", rel.escape_debug()),
        ));
    }
    Ok(names)
}
