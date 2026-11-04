//! A hold on a file: the liveness mark of a launch. Whoever holds it exclusively is alive; a child that inherits the
//! open file keeps the hold after its parent has closed its own copy, and the OS ends it when the last holder is gone,
//! killed or not.
//!
//! Windows: the file is open with share mode 0, so no other open of it succeeds. (A `LockFileEx` lock does not do:
//! it belongs to the handle that took it, not to the file, and an inherited handle does not carry it. Measured.)
//! Elsewhere: an exclusive `flock`, which belongs to the open file and so to every duplicate of it.

use std::fmt;
use std::fs::File;
use std::io;

use super::FileId;

/// The suffix of the name a claim file is given just before it is deleted (`<claim file>.<16 hex>.del`, the digits
/// drawn for that delete), where the host can only free a name by renaming the file away: Windows, where a deleted
/// file keeps its name until its last handle closes (with POSIX semantics, where the file system has them, until the
/// file object closes). A deleter that dies in between leaves the name; the sweep deletes it.
pub(crate) const DELETING_SUFFIX: &str = ".del";

/// An exclusive hold on a file, and the open file that carries it.
pub(crate) trait Hold: Send + fmt::Debug {
    /// The held file. A duplicate of it, given to a child to inherit, carries the hold on.
    fn file(&self) -> &File;

    /// The identity of the held file, read from the open file and not from a name: a name can be given to another file
    /// while the hold is kept.
    fn id(&self) -> io::Result<FileId>;

    /// Deletes the held file; the hold ends with it. The name is free when this returns, whoever else has the open
    /// file: where a name outlives the deletion of its file, the file is renamed to `<name>.<16 hex>.del` first. The file is
    /// gone for good only once nobody else holds a duplicate.
    fn delete(self: Box<Self>) -> io::Result<()>;
}

/// How a try to take a hold went. Nothing waits.
#[derive(Debug)]
pub(crate) enum HoldOutcome {
    Taken(Box<dyn Hold>),
    /// Somebody else holds the file.
    Busy,
    /// There is no such file.
    Absent,
}
