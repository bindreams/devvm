//! The runtime directory on Windows: the user's alone, from the moment it exists.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::marker::PhantomData;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    LookupAccountSidW, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SID_NAME_USE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
};

use super::user::{current_sid, sid_string};
use super::wide;

/// Makes `dir` with owner and DACL set in the one call that creates it: the owner is the user and the DACL is
/// protected (nothing inherited from `C:\ProgramData`, where Users may create entries) with one entry, full access for
/// the user. A directory that exists is accepted if the user owns it.
pub(super) fn create(dir: &Path) -> io::Result<()> {
    let sid = current_sid()?;
    let path = wide(dir)?;
    let descriptor = Descriptor::owner_only(true)?;
    let attributes = descriptor.attributes();
    // SAFETY: `path` is NUL-terminated and `attributes` points at the live descriptor.
    let made = unsafe { CreateDirectoryW(path.as_ptr(), attributes.as_raw()) };
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

/// A security descriptor parsed from its SDDL text.
pub(crate) struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    pub(crate) fn from_sddl(sddl: &str) -> io::Result<Descriptor> {
        let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `sddl` is NUL-terminated; on success `descriptor` is allocated for `LocalFree`.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::other(format!(
                "ConvertStringSecurityDescriptorToSecurityDescriptorW failed ({})",
                io::Error::last_os_error()
            )));
        }
        Ok(Descriptor(descriptor))
    }

    /// The descriptor of an object that only the user owns and can use: the user is the owner, and the DACL is
    /// protected (nothing inherited) with one entry, full access for the user. `inherit` makes that entry reach the
    /// children of a directory too.
    pub(crate) fn owner_only(inherit: bool) -> io::Result<Descriptor> {
        let sid = current_sid()?;
        let flags = if inherit { "OICI" } else { "" };
        Descriptor::from_sddl(&format!("O:{sid}D:P(A;{flags};FA;;;{sid})"))
    }

    /// Valid while `self` lives.
    pub(crate) fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.0
    }

    /// The attributes that create an object with this descriptor; not inheritable by a child process.
    pub(crate) fn attributes(&self) -> Attributes<'_> {
        Attributes {
            raw: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.0,
                bInheritHandle: 0,
            },
            descriptor: PhantomData,
        }
    }
}

/// [`SECURITY_ATTRIBUTES`] that point at a [`Descriptor`] and cannot outlive it.
pub(crate) struct Attributes<'a> {
    raw: SECURITY_ATTRIBUTES,
    descriptor: PhantomData<&'a Descriptor>,
}

impl Attributes<'_> {
    pub(crate) fn as_raw(&self) -> &SECURITY_ATTRIBUTES {
        &self.raw
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by the conversion, which documents `LocalFree`.
        unsafe { LocalFree(self.0) };
    }
}

/// The owner of an open file system object, from `GetSecurityInfo`.
pub(crate) struct Owner {
    descriptor: PSECURITY_DESCRIPTOR,
    sid: PSID,
}

impl Owner {
    pub(crate) fn of(handle: &File) -> io::Result<Owner> {
        let mut owner = Owner {
            descriptor: null_mut(),
            sid: null_mut(),
        };
        // SAFETY: the handle is open with `READ_CONTROL`; the out-pointers are valid, and a null group, DACL and SACL
        // pointer are allowed because those parts are not requested.
        let status = unsafe {
            GetSecurityInfo(
                handle.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner.sid,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut owner.descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(owner)
    }

    pub(crate) fn sid(&self) -> io::Result<String> {
        sid_string(self.sid)
    }

    /// `DOMAIN\name`, or the SID string if the account has no name.
    fn label(&self) -> String {
        let (mut name_len, mut domain_len) = (0u32, 0u32);
        let mut kind: SID_NAME_USE = 0;
        // SAFETY: a null buffer with zero length asks for the lengths needed; `sid` is the owner SID of the descriptor.
        unsafe {
            LookupAccountSidW(
                null(),
                self.sid,
                null_mut(),
                &mut name_len,
                null_mut(),
                &mut domain_len,
                &mut kind,
            )
        };
        let (mut name, mut domain) = (vec![0u16; name_len as usize], vec![0u16; domain_len as usize]);
        // SAFETY: both buffers have the lengths passed.
        let found = unsafe {
            LookupAccountSidW(
                null(),
                self.sid,
                name.as_mut_ptr(),
                &mut name_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut kind,
            )
        };
        if found == 0 {
            return self.sid().unwrap_or_else(|_| "an unknown account".to_owned());
        }
        let text = |units: &[u16]| OsString::from_wide(units).to_string_lossy().into_owned();
        let (name, domain) = (text(&name[..name_len as usize]), text(&domain[..domain_len as usize]));
        if domain.is_empty() {
            name
        } else {
            format!("{domain}\\{name}")
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated by `GetSecurityInfo`, which documents `LocalFree`.
        unsafe { LocalFree(self.descriptor) };
    }
}
