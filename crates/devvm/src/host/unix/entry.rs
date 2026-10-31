//! [`Host::open_entry`](crate::host::Host::open_entry) on Unix: `openat` relative to the handle of the directory above,
//! with `O_NOFOLLOW`, so that a link at a name is never followed, and nothing is looked at by path twice.

use std::fs::File;
use std::io;
use std::path::Path;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, open, openat, statat};
use rustix::io::Errno;

use crate::host::entry::components;
use crate::host::{Entry, is_absent};

#[cfg(test)]
#[path = "entry_tests.rs"]
mod entry_tests;

pub(super) fn open_entry(root: &Path, rel: &str) -> io::Result<Entry> {
    let names = components(rel)?;
    let (last, parents) = names.split_last().expect("a path has a name");
    let mut directory = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    for (i, name) in parents.iter().enumerate() {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match openat(&directory, *name, flags, Mode::empty()) {
            Ok(next) => directory = next,
            // A link refuses `O_NOFOLLOW`, as `ELOOP` or, with `O_DIRECTORY`, as `ENOTDIR`; which one depends on the OS.
            Err(errno) => {
                return match file_type_of(&directory, name) {
                    Some(FileType::Symlink) => Ok(Entry::BelowSymlink(names[..=i].join("/"))),
                    _ => absent_or_failed(errno),
                };
            }
        }
    }
    // `O_NONBLOCK` so that opening a FIFO cannot wait for a writer, `O_NOCTTY` so that a terminal does not become ours.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    let fd = match openat(&directory, *last, flags, Mode::empty()) {
        Ok(fd) => fd,
        // Not openable, but its type still says what it is: a link (`ELOOP`, `EMLINK` on FreeBSD) or a socket (`ENXIO`).
        Err(errno) => {
            return match file_type_of(&directory, last) {
                Some(FileType::Symlink) => Ok(Entry::Symlink),
                Some(FileType::Socket) => Ok(Entry::Other("a socket")),
                _ => absent_or_failed(errno),
            };
        }
    };
    let file_type = FileType::from_raw_mode(fstat(&fd)?.st_mode);
    Ok(match file_type {
        FileType::RegularFile => Entry::File(File::from(fd)),
        FileType::Directory => Entry::Directory,
        FileType::Symlink => Entry::Symlink,
        FileType::Fifo => Entry::Other("a FIFO"),
        FileType::Socket => Entry::Other("a socket"),
        FileType::CharacterDevice => Entry::Other("a character device"),
        FileType::BlockDevice => Entry::Other("a block device"),
        FileType::Unknown => Entry::Other("of a type this host does not know"),
    })
}

/// The type of `name` in `directory` without following it, if it can be read.
fn file_type_of(directory: &impl rustix::fd::AsFd, name: &str) -> Option<FileType> {
    let stat = statat(directory, name, AtFlags::SYMLINK_NOFOLLOW).ok()?;
    Some(FileType::from_raw_mode(stat.st_mode))
}

fn absent_or_failed(errno: Errno) -> io::Result<Entry> {
    let error = io::Error::from(errno);
    if is_absent(&error) {
        Ok(Entry::Absent)
    } else {
        Err(error)
    }
}
