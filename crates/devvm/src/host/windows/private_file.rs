//! A private file on Windows: the user's alone, from the moment it exists.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC,
};

use super::create_file::open;
use super::security::Descriptor;
use crate::host::MakePrivateOutcome;

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
/// `READ_CONTROL`); its contents are not read or changed.
pub(super) fn restrict(path: &Path) -> io::Result<MakePrivateOutcome> {
    let open_for = |access| {
        OpenOptions::new()
            .access_mode(access)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    };
    if Descriptor::user_only_file()?.is_the_dacl_of(&open_for(READ_CONTROL)?)? {
        return Ok(MakePrivateOutcome::Unchanged);
    }
    Descriptor::user_only_file()?.protect(&open_for(WRITE_DAC | READ_CONTROL)?)?;
    Ok(MakePrivateOutcome::Changed)
}
