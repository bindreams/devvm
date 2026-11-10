//! Security descriptors and the owner of a file system object on Windows.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    LookupAccountSidW, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE,
};

use super::user::{current_sid, sid_string};

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

    /// The descriptor of an object that only the current user owns and can use: the DACL is protected (nothing
    /// inherited) with one entry, full access for the user. `inherit` makes that entry reach the children of a
    /// directory too.
    pub(crate) fn owner_only(inherit: bool) -> io::Result<Descriptor> {
        let sid = current_sid()?;
        let flags = if inherit { "OICI" } else { "" };
        Descriptor::from_sddl(&format!("O:{sid}D:P(A;{flags};FA;;;{sid})"))
    }

    /// Valid while `self` lives.
    pub(crate) fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.0
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
    pub(super) fn label(&self) -> String {
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
