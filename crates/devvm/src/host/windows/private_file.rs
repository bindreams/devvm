//! A private file on Windows: the user's alone, from the moment it exists.

use std::fs::File;
use std::io;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
};

use super::create_file::open;
use super::security::Descriptor;

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
