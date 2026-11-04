use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, GetAce,
    GetAclInformation, GetSecurityDescriptorControl, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SE_DACL_PROTECTED,
};

use crate::host::windows::user;

/// The owner and the DACL of a path, read the way the OS reports them.
pub struct Security {
    descriptor: PSECURITY_DESCRIPTOR,
    owner: PSID,
    dacl: *mut ACL,
}

impl Security {
    pub fn of(path: &Path) -> Security {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut security = Security {
            descriptor: std::ptr::null_mut(),
            owner: std::ptr::null_mut(),
            dacl: std::ptr::null_mut(),
        };
        // SAFETY: `wide` is NUL-terminated and the out-pointers are valid; a null group and SACL pointer are allowed.
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut security.owner,
                std::ptr::null_mut(),
                &mut security.dacl,
                std::ptr::null_mut(),
                &mut security.descriptor,
            )
        };
        assert_eq!(status, 0, "GetNamedSecurityInfoW({})", path.display());
        security
    }

    pub fn owner(&self) -> String {
        user::sid_string(self.owner).unwrap()
    }

    pub fn dacl_is_protected(&self) -> bool {
        let (mut control, mut revision) = (0u16, 0u32);
        // SAFETY: the descriptor is the live one from `GetNamedSecurityInfoW`.
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(self.descriptor, &mut control, &mut revision) },
            0
        );
        control & SE_DACL_PROTECTED != 0
    }

    /// The SIDs of the DACL's entries, every one of which must be a plain allow entry.
    pub fn allowed_sids(&self) -> Vec<String> {
        assert!(!self.dacl.is_null(), "a null DACL grants everyone everything");
        let mut size = ACL_SIZE_INFORMATION {
            AceCount: 0,
            AclBytesInUse: 0,
            AclBytesFree: 0,
        };
        // SAFETY: the DACL is valid and the buffer is the size passed.
        let read = unsafe {
            GetAclInformation(
                self.dacl,
                (&raw mut size).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };
        assert_ne!(read, 0);
        (0..size.AceCount)
            .map(|index| {
                let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
                // SAFETY: `index` is below the entry count.
                assert_ne!(unsafe { GetAce(self.dacl, index, &mut ace) }, 0);
                // SAFETY: every entry starts with a header.
                let kind = unsafe { std::ptr::read_unaligned(ace.cast::<ACE_HEADER>()) }.AceType;
                assert_eq!(kind, 0, "entry {index} is not a plain allow entry");
                // SAFETY: a type-0 entry is an `ACCESS_ALLOWED_ACE`, whose SID starts at `SidStart`.
                let sid = unsafe { (&raw mut (*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart).cast::<std::ffi::c_void>() };
                user::sid_string(sid).unwrap()
            })
            .collect()
    }
}

impl Drop for Security {
    fn drop(&mut self) {
        // SAFETY: allocated by `GetNamedSecurityInfoW`, which documents `LocalFree`.
        unsafe { LocalFree(self.descriptor) };
    }
}
