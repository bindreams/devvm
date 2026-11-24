//! A private file on Windows: the user's alone, from the moment it exists.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC,
};

use super::create_file::open;
use super::file_id::of_handle;
use super::hold::{LINK_OPEN, refuse_link};
use super::security::Descriptor;
use crate::host::{MakePrivateError, MakePrivateOutcome};

/// Creates `path`, which must not exist (`AlreadyExists` otherwise), open for writing, with the descriptor of
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
        FILE_GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        CREATE_NEW,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
        Some(&descriptor),
    )?)
}

/// Gives the existing file `path` the DACL of [`Descriptor::user_only_file`] in place of whatever it has, inherited or
/// not, unless it has that DACL already. The file is opened without following a link, first for `READ_CONTROL` to look,
/// and only if it must change for `WRITE_DAC` as well (`SetSecurityInfo` answers access denied without
/// `READ_CONTROL`). A link is refused, and so is a second open that reaches another file than the first: the first
/// handle stays open, so the id it has cannot go to another file.
pub(super) fn restrict(path: &Path) -> Result<MakePrivateOutcome, MakePrivateError> {
    let open_for = |access| {
        OpenOptions::new()
            .access_mode(access | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(LINK_OPEN)
            .open(path)
    };
    let looked_at = open_for(READ_CONTROL).map_err(MakePrivateError::Check)?;
    refuse_link(&looked_at, path).map_err(MakePrivateError::Check)?;
    let user_only = Descriptor::user_only_file().map_err(MakePrivateError::Check)?;
    if user_only.is_the_dacl_of(&looked_at).map_err(MakePrivateError::Check)? {
        return Ok(MakePrivateOutcome::Unchanged);
    }
    let to_change = open_for(WRITE_DAC | READ_CONTROL).map_err(MakePrivateError::Change)?;
    refuse_link(&to_change, path).map_err(MakePrivateError::Check)?;
    if of_handle(&looked_at).map_err(MakePrivateError::Check)?
        != of_handle(&to_change).map_err(MakePrivateError::Check)?
    {
        return Err(MakePrivateError::Check(io::Error::other(format!(
            "{} is another file than the one looked at",
            path.display()
        ))));
    }
    user_only.protect(&to_change).map_err(MakePrivateError::Change)?;
    Ok(MakePrivateOutcome::Changed)
}
