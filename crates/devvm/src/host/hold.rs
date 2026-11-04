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
use std::path::{Path, PathBuf};

use super::FileId;
use crate::random;

// The private names of a claim file ===================================================================================
// Each has its builder and its parser side by side, so the sweep of the runtime directory reads back what a launch or
// a delete wrote.

/// The suffix of the name a claim file has while it is made (`<claim file>.new`), where the host cannot create a file
/// and hold it in one step: off Windows, the file is made and locked under this name and then linked to the claim
/// file's. A launch that dies in between leaves the name; the sweep deletes it.
const NEW_SUFFIX: &str = ".new";

/// The name the claim file `claim` has while it is made.
pub(crate) fn new_name(claim: &Path) -> PathBuf {
    let mut name = claim.as_os_str().to_owned();
    name.push(NEW_SUFFIX);
    PathBuf::from(name)
}

/// The claim file's name, if `name` is its name while it is made: the inverse of [`new_name`].
pub(crate) fn claim_of_new_name(name: &str) -> Option<&str> {
    name.strip_suffix(NEW_SUFFIX)
}

/// The suffix of the name a claim file is given just before it is deleted (`<claim file>.<16 hex>.del`, the digits
/// drawn for that delete), where the host can only free a name by renaming the file away: Windows, where a deleted
/// file keeps its name until its last handle closes (with POSIX semantics, where the file system has them, until the
/// file object closes). A deleter that dies in between leaves the name; the sweep deletes it.
const DELETING_SUFFIX: &str = ".del";

/// The deleting name of the claim file `claim`, for the digits `draw` is the hex of.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "only the Windows host renames a claim file away; the tests use it everywhere"
    )
)]
pub(crate) fn deleting_name(claim: &Path, draw: [u8; 8]) -> PathBuf {
    let mut name = claim.as_os_str().to_owned();
    name.push(format!(".{}{DELETING_SUFFIX}", hex::encode(draw)));
    PathBuf::from(name)
}

/// The claim file's name, if `name` is its deleting name: the inverse of [`deleting_name`].
pub(crate) fn claim_of_deleting_name(name: &str) -> Option<&str> {
    let (claim, digits) = name.strip_suffix(DELETING_SUFFIX)?.rsplit_once('.')?;
    random::is_hex(digits, 8).then_some(claim)
}

// The hold ============================================================================================================

/// An exclusive hold on a file, and the open file that carries it.
pub(crate) trait Hold: Send + fmt::Debug {
    /// The held file. A duplicate of it, given to a child to inherit, carries the hold on.
    fn file(&self) -> &File;

    /// The identity of the held file, read from the open file and not from a name: a name can be given to another file
    /// while the hold is kept.
    fn id(&self) -> io::Result<FileId>;

    /// Deletes the held file; the hold ends with it. The name is free when this returns, whoever else has the open
    /// file: where a name outlives the deletion of its file, the file is renamed to its [`deleting_name`] first. The
    /// file is gone for good only once nobody else holds a duplicate.
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

#[cfg(test)]
#[path = "hold_tests.rs"]
mod hold_tests;
