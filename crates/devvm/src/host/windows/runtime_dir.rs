//! The runtime directory on Windows: the user's alone, from the moment it exists.

use std::fs::OpenOptions;
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
};

use super::security::{Descriptor, Owner};
use super::user::current_sid;
use super::wide;

/// Makes `dir` with [`Descriptor::owner_only`] set in the call that creates it, so nothing is inherited from
/// `C:\ProgramData`, where Users may create entries. A directory that exists is accepted if the user owns it.
pub(super) fn create(dir: &Path) -> io::Result<()> {
    let sid = current_sid()?;
    let path = wide(dir)?;
    let descriptor = Descriptor::owner_only()?;
    // SAFETY: `path` is NUL-terminated and `attributes` is valid for the call.
    let made = descriptor.with_attributes(|attributes| unsafe { CreateDirectoryW(path.as_ptr(), attributes) });
    let error = io::Error::last_os_error();
    drop(descriptor);
    if made != 0 {
        return Ok(());
    }
    if error.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) {
        return accept_existing(dir, &sid);
    }
    Err(error)
}

/// [`Host::accept_runtime_dir`].
pub(super) fn accept(dir: &Path) -> io::Result<()> {
    accept_existing(dir, &current_sid()?)
}

/// A directory at `dir` that the user owns and that is no link: `AlreadyExists` for a file or a link, and
/// `PermissionDenied` naming the owner for another account's directory, or saying the owner cannot be read when the
/// DACL denies `READ_CONTROL` (the owner is implicitly granted it, so this is not the user's directory) or its deletion
/// is pending. Any other failure names the directory.
///
/// The path is looked up once. What is checked (no link, a directory) and whose it is are both read from the one
/// handle, which opens a link itself and does not follow it, so the directory cannot be swapped between the checks.
fn accept_existing(dir: &Path, me: &str) -> io::Result<()> {
    let handle = OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
        .map_err(|e| at(dir, e))?;
    let metadata = handle.metadata().map_err(|e| at(dir, e))?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} is a link, not a directory", dir.display()),
        ));
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} is not a directory", dir.display()),
        ));
    }
    let owner = Owner::of(&handle).map_err(|e| at(dir, e))?;
    if owner.sid().map_err(|e| at(dir, e))? == me {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{} is owned by {}, not by you", dir.display(), owner.label()),
    ))
}

/// `error` from a call on `dir`, with the directory named. `ERROR_ACCESS_DENIED` is `PermissionDenied` saying the owner
/// cannot be read: a directory the user does not own, or one whose deletion is pending.
fn at(dir: &Path, error: io::Error) -> io::Error {
    if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        return io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("cannot read the owner of {} ({error})", dir.display()),
        );
    }
    io::Error::new(error.kind(), format!("{}: {error}", dir.display()))
}
