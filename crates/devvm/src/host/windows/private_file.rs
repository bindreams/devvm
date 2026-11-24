//! A private file on Windows: the user's alone, from the moment it exists.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA, READ_CONTROL, ReOpenFile, WRITE_DAC,
};

use super::create_file::open;
use super::hold::{LINK_OPEN, refuse_link};
use super::security::Descriptor;
use crate::host::{MakePrivateError, MakePrivateOutcome};

/// Creates `path`, which must not exist (`AlreadyExists` otherwise), open for appending, with the descriptor of
/// [`Descriptor::user_only_file`]: the token's default owner (the user, or Administrators for an elevated token), and
/// a protected DACL with one entry, full access for the user.
///
/// OpenSSH for Windows (`check_secure_file_permission`, `contrib/win32/win32compat/w32-sshfileperm.c`) requires the
/// owner to be the user, Administrators, SYSTEM or TrustedInstaller, and refuses a private key with an allow entry for
/// any other trustee. A DACL inherited from a directory that grants `BUILTIN\Users` read fails that rule.
pub(super) fn create_new(path: &Path) -> io::Result<File> {
    let descriptor = Descriptor::user_only_file()?;
    Ok(open(
        path,
        // Append without the plain write right, as std's `append(true)`.
        FILE_GENERIC_WRITE & !FILE_WRITE_DATA,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        CREATE_NEW,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
        Some(&descriptor),
    )?)
}

/// Gives the existing file `path` the DACL of [`Descriptor::user_only_file`] in place of whatever it has, inherited or
/// not, unless it has that DACL already. The name is looked up once, without following a link, for `READ_CONTROL`: a
/// link and a directory are refused, and a file found private is left as it is. Any other file is reopened through that
/// handle for `READ_CONTROL` and `WRITE_DAC` (`SetSecurityInfo` answers access denied without `READ_CONTROL`), so the
/// check and the change are on one file. A refused reopen or a failed change is a [`Change`](MakePrivateError::Change)
/// error.
pub(super) fn restrict(path: &Path) -> Result<MakePrivateOutcome, MakePrivateError> {
    let user_only = Descriptor::user_only_file().map_err(MakePrivateError::Check)?;
    let file = OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(LINK_OPEN)
        .open(path)
        .map_err(MakePrivateError::Check)?;
    refuse_link(&file, path).map_err(MakePrivateError::Check)?;
    if user_only.is_the_dacl_of(&file).map_err(MakePrivateError::Check)? {
        return Ok(MakePrivateOutcome::Unchanged);
    }
    let writable = reopen_for_dacl(&file).map_err(MakePrivateError::Change)?;
    user_only.protect(&writable).map_err(MakePrivateError::Change)?;
    Ok(MakePrivateOutcome::Changed)
}

/// Opens the file `file` is a handle to again, for `READ_CONTROL` and `WRITE_DAC`, sharing everything: no second lookup
/// of a name.
fn reopen_for_dacl(file: &File) -> io::Result<File> {
    // SAFETY: `file`'s handle is open for the call; no pointers.
    let handle = unsafe {
        ReOpenFile(
            file.as_raw_handle() as HANDLE,
            READ_CONTROL | WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a new handle that nothing else owns.
    Ok(unsafe { File::from_raw_handle(handle as _) })
}
