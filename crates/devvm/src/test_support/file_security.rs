use std::fs::OpenOptions;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, GetAce,
    GetAclInformation, GetSecurityDescriptorControl, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, READ_CONTROL,
};

use crate::host::windows::security::{Descriptor, Owner};
use crate::host::windows::{user, wide};

/// Replaces the DACL of `path` with the one `sddl` describes (`D:...`), protected: nothing is inherited.
pub fn set_protected_dacl(path: &Path, sddl: &str) {
    let wide_path = wide(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let descriptor = Descriptor::from_sddl(sddl).expect("the DACL does not parse");
    let (mut present, mut defaulted) = (0, 0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: the descriptor is live and the out-pointers are valid.
    let got = unsafe { GetSecurityDescriptorDacl(descriptor.as_ptr(), &mut present, &mut dacl, &mut defaulted) };
    assert!(got != 0 && present != 0, "the descriptor has no DACL");
    // SAFETY: `wide_path` is NUL-terminated and `dacl` points into the live descriptor; a null owner, group and SACL
    // are allowed because those parts are not set.
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide_path.as_ptr().cast_mut(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(status, 0, "SetNamedSecurityInfoW({})", path.display());
}

/// The owner and the DACL of a path, read the way the OS reports them. The owner comes from the production reader.
pub struct Security {
    owner: Owner,
    descriptor: PSECURITY_DESCRIPTOR,
    dacl: *mut ACL,
}

impl Security {
    pub fn of(path: &Path) -> Security {
        // Opened as the runtime directory is when its owner is checked: links are not followed.
        let handle = OpenOptions::new()
            .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let owner = Owner::of(&handle).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let wide = wide(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let mut security = Security {
            owner,
            descriptor: std::ptr::null_mut(),
            dacl: std::ptr::null_mut(),
        };
        // SAFETY: `wide` is NUL-terminated and the out-pointers are valid; a null owner, group and SACL pointer are
        // allowed because those parts are not requested.
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
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
        self.owner.sid().unwrap()
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
