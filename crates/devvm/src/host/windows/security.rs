//! Security descriptors and the owner of a file system object on Windows.

use std::ffi::{OsString, c_void};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
    SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, GetAce, GetAclInformation,
    GetSecurityDescriptorControl, GetSecurityDescriptorDacl, LookupAccountSidW, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
    SID_NAME_USE,
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

    /// The descriptor of a directory that only the current user owns and can use: the owner is the user and the DACL is
    /// protected (nothing inherited) with one entry, full access for the user, which the directory's children inherit.
    pub(crate) fn owner_only() -> io::Result<Descriptor> {
        let sid = current_sid()?;
        Descriptor::from_sddl(&format!("O:{sid}D:P(A;OICI;FA;;;{sid})"))
    }

    /// The descriptor of a file that only the current user can use: no owner, so the creator's default owner applies,
    /// and a protected DACL (nothing inherited) with one entry, full access for the user.
    pub(crate) fn user_only_file() -> io::Result<Descriptor> {
        let sid = current_sid()?;
        Descriptor::from_sddl(&format!("D:P(A;;FA;;;{sid})"))
    }

    /// Calls `with` with this descriptor's DACL, which lives for the call only.
    pub(crate) fn with_dacl<R>(&self, with: impl FnOnce(*mut ACL) -> R) -> io::Result<R> {
        let (mut present, mut defaulted) = (0, 0);
        let mut dacl: *mut ACL = null_mut();
        // SAFETY: the descriptor is live and the out-pointers are valid.
        let got = unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted) };
        if got == 0 {
            return Err(io::Error::last_os_error());
        }
        if present == 0 {
            return Err(io::Error::other("the descriptor has no DACL"));
        }
        Ok(with(dacl))
    }

    /// Replaces the DACL of the object `handle` names with this descriptor's, protected: nothing is inherited. The handle
    /// must be open with `WRITE_DAC` and `READ_CONTROL`.
    pub(crate) fn protect(&self, handle: &File) -> io::Result<()> {
        let status = self.with_dacl(|dacl| {
            // SAFETY: the handle is open; `dacl` points into this live descriptor; a null owner, group and SACL are
            // allowed because those parts are not set.
            unsafe {
                SetSecurityInfo(
                    handle.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    dacl,
                    null(),
                )
            }
        })?;
        match status {
            ERROR_SUCCESS => Ok(()),
            code => Err(io::Error::from_raw_os_error(code as i32)),
        }
    }

    /// Whether the object `handle` names has the DACL this descriptor has: both protected or both not, and the same
    /// entries in the same order. A null DACL, which grants everyone everything, is never the same. The handle must be
    /// open with `READ_CONTROL`; nothing is written.
    pub(crate) fn is_the_dacl_of(&self, handle: &File) -> io::Result<bool> {
        let object = ObjectDacl::of(handle)?;
        // SAFETY: both descriptors are live, and nothing changes them meanwhile.
        let same_control = unsafe { is_protected(self.0)? == is_protected(object.descriptor)? };
        if !same_control {
            return Ok(false);
        }
        self.with_dacl(|mine| {
            if mine.is_null() || object.dacl.is_null() {
                return Ok(false);
            }
            // SAFETY: both ACLs lie in live descriptors that nothing changes meanwhile.
            let (mine, theirs) = unsafe { (acl_entries(mine)?, acl_entries(object.dacl)?) };
            Ok(mine == theirs)
        })?
    }

    /// Calls `with` with attributes that carry this descriptor and no inheritable handle; they live for the call only.
    pub(crate) fn with_attributes<R>(&self, with: impl FnOnce(&SECURITY_ATTRIBUTES) -> R) -> R {
        with(&SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        })
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by the conversion, which documents `LocalFree`.
        unsafe { LocalFree(self.0) };
    }
}

/// The DACL of an open file system object, from `GetSecurityInfo`.
struct ObjectDacl {
    descriptor: PSECURITY_DESCRIPTOR,
    dacl: *mut ACL,
}

impl ObjectDacl {
    /// The handle must be open with `READ_CONTROL`.
    fn of(handle: &File) -> io::Result<ObjectDacl> {
        let mut object = ObjectDacl {
            descriptor: null_mut(),
            dacl: null_mut(),
        };
        // SAFETY: the handle is open with `READ_CONTROL`; the out-pointers are valid, and a null owner, group and SACL
        // pointer are allowed because those parts are not requested.
        let status = unsafe {
            GetSecurityInfo(
                handle.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut object.dacl,
                null_mut(),
                &mut object.descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(object)
    }
}

impl Drop for ObjectDacl {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated by `GetSecurityInfo`, which documents `LocalFree`.
        unsafe { LocalFree(self.descriptor) };
    }
}

/// Whether the DACL of `descriptor` is protected: it inherits nothing.
///
/// # Safety
///
/// `descriptor` must point to a valid security descriptor that stays valid for the call.
unsafe fn is_protected(descriptor: PSECURITY_DESCRIPTOR) -> io::Result<bool> {
    let (mut control, mut revision) = (0u16, 0u32);
    // SAFETY: `descriptor` is valid, by this function's contract, and the out-pointers are valid.
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(control & SE_DACL_PROTECTED != 0)
}

/// The entries of `acl`, each as the bytes it occupies.
///
/// # Safety
///
/// `acl` must point to a valid ACL that stays valid and unchanged for `'a`.
unsafe fn acl_entries<'a>(acl: *const ACL) -> io::Result<Vec<&'a [u8]>> {
    let mut size = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    // SAFETY: `acl` is valid, by this function's contract, and the buffer is the size passed.
    let read = unsafe {
        GetAclInformation(
            acl,
            (&raw mut size).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    (0..size.AceCount)
        .map(|index| {
            let mut ace: *mut c_void = null_mut();
            // SAFETY: `index` is below the entry count.
            if unsafe { GetAce(acl, index, &mut ace) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: every entry starts with a header, in a byte buffer that may not align it.
            let len = usize::from(unsafe { std::ptr::read_unaligned(ace.cast::<ACE_HEADER>()) }.AceSize);
            // SAFETY: the header's `AceSize` is the length of the entry, which lies inside the valid ACL.
            Ok(unsafe { std::slice::from_raw_parts(ace.cast::<u8>(), len) })
        })
        .collect()
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
        // SAFETY: `sid` points into the descriptor `GetSecurityInfo` allocated, which `self` owns until it is dropped.
        unsafe { sid_string(self.sid) }
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
