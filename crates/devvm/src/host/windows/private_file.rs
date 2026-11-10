//! A private file on Windows: the user's alone, from the moment it exists.

use std::fs::File;
use std::io;
use std::path::Path;

use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
};

use super::hold::open;
use super::runtime_dir::Descriptor;
use super::user::current_sid;

/// Creates `path`, which must not exist (`AlreadyExists` otherwise), open for writing. The owner and the DACL are set
/// by the call that creates the file: the owner is the user and the DACL is protected (nothing inherited) with one
/// entry, full access for the user.
///
/// OpenSSH for Windows (`check_secure_file_permission`, `contrib/win32/win32compat/w32-sshfileperm.c`) requires the
/// owner to be the user, Administrators, SYSTEM or TrustedInstaller, and refuses a private key with an allow entry for
/// any other trustee. A DACL inherited from a directory that grants `BUILTIN\Users` read fails that rule.
pub(super) fn create_new(path: &Path) -> io::Result<File> {
    let sid = current_sid()?;
    let descriptor = Descriptor::from_sddl(&format!("O:{sid}D:P(A;;FA;;;{sid})"))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    // SAFETY (`attributes`): points at the live descriptor, which outlives the call.
    Ok(open(
        path,
        FILE_GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        CREATE_NEW,
        FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
        &attributes,
    )?)
}
