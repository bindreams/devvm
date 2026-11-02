//! A hold off Windows: an exclusive `flock`, which belongs to the open file and so to every duplicate of it, an
//! inherited descriptor included.

use std::ffi::OsString;
use std::fs::{File, Metadata, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::host::{FileId, Hold, HoldOutcome};

/// The identity of the object a `Metadata` describes, as the host gives it (`FileModes::identity`).
pub(super) type Identity<'a> = &'a dyn Fn(&Metadata) -> Option<FileId>;

#[derive(Debug)]
struct FlockHold {
    file: File,
    path: PathBuf,
    /// Read from the descriptor when the hold was taken; `None` for a host that cannot tell files apart.
    id: Option<FileId>,
}

impl FlockHold {
    fn of(file: File, path: &Path, identity: Identity) -> std::io::Result<FlockHold> {
        let id = identity(&file.metadata()?);
        Ok(FlockHold {
            file,
            path: path.to_owned(),
            id,
        })
    }
}

/// `error` from linking a claim file to its name, as the caller gets it: a file system that does not allow hard links
/// answers `EPERM` or `ENOTSUP`, and is told so, with the way to another directory. Any other failure (a name that
/// exists, among them) is returned as it was.
fn link_error(error: std::io::Error, path: &Path) -> std::io::Error {
    if !matches!(error.kind(), ErrorKind::Unsupported | ErrorKind::PermissionDenied) {
        return error;
    }
    let dir = path.parent().unwrap_or(path);
    std::io::Error::new(
        error.kind(),
        format!(
            "{} does not allow hard links ({error}), which devvm needs there; set XDG_RUNTIME_DIR to use another directory",
            dir.display()
        ),
    )
}

/// `path` with `.new` after it: where a hold is made before it has its name.
fn private_name(path: &Path) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(".new");
    PathBuf::from(name)
}

/// Creates `path` with `options` (private to the user) and holds it. The name never exists without the hold: the file
/// is made and locked under its private name, and then linked to `path`, which fails if `path` exists
/// (`AlreadyExists`, as does a file at the private name, which is another launch's claim in the making).
pub(super) fn new(mut options: OpenOptions, path: &Path, identity: Identity) -> std::io::Result<Box<dyn Hold>> {
    let private = private_name(path);
    let file = options.read(true).write(true).create_new(true).open(&private)?;
    let made = match file.try_lock() {
        Ok(()) => FlockHold::of(file, path, identity).and_then(|hold| {
            std::fs::hard_link(&private, path)
                .map(|()| hold)
                .map_err(|e| link_error(e, path))
        }),
        Err(TryLockError::WouldBlock) => Err(std::io::Error::other(format!(
            "{} was locked by another process as it was created",
            private.display()
        ))),
        Err(TryLockError::Error(e)) => Err(e),
    };
    // The private name goes whether or not the name was linked.
    let unlinked = std::fs::remove_file(&private);
    match (made, unlinked) {
        (Ok(hold), Ok(())) => Ok(Box::new(hold)),
        (Ok(hold), Err(e)) => {
            drop(hold);
            let _ = std::fs::remove_file(path);
            Err(e)
        }
        (Err(e), _) => Err(e),
    }
}

/// Holds the file at `path` if nobody else does. `options` open it and refuse a link at the name (`FileModes::hold_options`).
pub(super) fn existing(options: OpenOptions, path: &Path, identity: Identity) -> std::io::Result<HoldOutcome> {
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HoldOutcome::Absent),
        Err(e) => return Err(e),
    };
    match file.try_lock() {
        Ok(()) => Ok(HoldOutcome::Taken(Box::new(FlockHold::of(file, path, identity)?))),
        Err(TryLockError::WouldBlock) => Ok(HoldOutcome::Busy),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

impl Hold for FlockHold {
    fn file(&self) -> &File {
        &self.file
    }

    fn id(&self) -> std::io::Result<FileId> {
        self.id
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Unsupported, "this build cannot tell files apart"))
    }

    /// Unlinks the name while the lock is still held, so nobody holds a file that has another's name. A name that is
    /// already gone is fine: a sweep and a record's cleanup can race for one claim.
    fn delete(self: Box<Self>) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

#[cfg(test)]
#[path = "hold_tests.rs"]
mod hold_tests;
