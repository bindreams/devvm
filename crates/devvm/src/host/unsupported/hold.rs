//! A hold off Windows: an exclusive `flock`, which belongs to the open file and so to every duplicate of it, an
//! inherited descriptor included.

use std::fs::{File, Metadata, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::host::{FileId, Hold, HoldOutcome, after_failed_write, is_absent, new_name, symlink_metadata_if_present};

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

/// Whether an error from linking a claim file says that the file system lacks hard links (`FileModes::lacks_hard_links`).
pub(super) type LacksHardLinks<'a> = &'a dyn Fn(&std::io::Error) -> bool;

/// `error` from linking a claim file to its name, as the caller gets it: a file system that does not allow hard links
/// is told so, with the way to another directory. Any other failure (a name that exists, a permission that is missing,
/// among them) is returned as it was.
fn link_error(error: std::io::Error, path: &Path, lacks_hard_links: LacksHardLinks) -> std::io::Error {
    if !lacks_hard_links(&error) {
        return error;
    }
    let dir = path.parent().unwrap_or(path);
    std::io::Error::new(
        error.kind(),
        format!(
            "{} does not allow hard links ({error}), which devvm needs there; set XDG_RUNTIME_DIR to use another \
             directory",
            dir.display()
        ),
    )
}

/// Creates and holds `path` (see `Host::hold_new`): made and locked as `<path>.new`, then hard-linked to `path`. A file
/// at either name is `AlreadyExists`, and the caller then draws another name: a `.new` that could not be removed is a
/// dead launch's leftover, which the sweep reclaims. Any other failure names the file it could not remove.
pub(super) fn new(
    mut options: OpenOptions,
    path: &Path,
    identity: Identity,
    lacks_hard_links: LacksHardLinks,
) -> std::io::Result<Box<dyn Hold>> {
    let private = new_name(path);
    let file = options.read(true).write(true).create_new(true).open(&private)?;
    let made = match file.try_lock() {
        Ok(()) => FlockHold::of(file, path, identity).and_then(|hold| {
            std::fs::hard_link(&private, path)
                .map(|()| hold)
                .map_err(|e| link_error(e, path, lacks_hard_links))
        }),
        Err(TryLockError::WouldBlock) => Err(std::io::Error::other(format!(
            "{} was locked by another process as it was created",
            private.display()
        ))),
        Err(TryLockError::Error(e)) => Err(e),
    };
    // The private name goes whether or not the name was linked. A name that cannot be removed is a dead launch's
    // leftover, and the sweep reclaims it (`.new` is swept, and so is a free claim file).
    let unlinked = std::fs::remove_file(&private);
    match (made, unlinked) {
        (Ok(hold), Ok(())) => Ok(Box::new(hold)),
        (Ok(hold), Err(e)) => {
            drop(hold);
            let e = std::io::Error::new(e.kind(), format!("cannot remove {}: {e}", private.display()));
            Err(after_failed_write(path, e, || std::fs::remove_file(path)))
        }
        (Err(e), unlinked) => Err(after_failed_write(&private, e, || unlinked)),
    }
}

fn is_a_link(path: &Path) -> std::io::Error {
    std::io::Error::new(ErrorKind::InvalidInput, format!("{} is a link", path.display()))
}

/// Holds the file at `path` if nobody else does. `options` open it and refuse a link at the name
/// (`FileModes::hold_options`).
pub(super) fn existing(options: OpenOptions, path: &Path, identity: Identity) -> std::io::Result<HoldOutcome> {
    // Looked at first for a host whose `options` cannot refuse a link.
    if symlink_metadata_if_present(path)?.is_some_and(|meta| meta.is_symlink()) {
        return Err(is_a_link(path));
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if is_absent(&e) => return Ok(HoldOutcome::Absent),
        // `options` refuse a link that appeared since the look, with an error that differs by system.
        Err(_) if matches!(symlink_metadata_if_present(path), Ok(Some(meta)) if meta.is_symlink()) => {
            return Err(is_a_link(path));
        }
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
            Err(e) if is_absent(&e) => Ok(()),
            other => other,
        }
    }
}

#[cfg(test)]
#[path = "hold_tests.rs"]
mod hold_tests;
