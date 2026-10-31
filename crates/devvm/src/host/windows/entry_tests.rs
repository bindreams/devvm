use std::fs::OpenOptions;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, DENY_ACCESS, EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, SE_FILE_OBJECT, SetEntriesInAclW,
    SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_WELL_KNOWN_GROUP,
};
use windows_sys::Win32::Security::{ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE, PSECURITY_DESCRIPTOR, PSID};
use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_LIST_DIRECTORY};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;

use crate::host::{Entry, Host};
use crate::test_support::fixtures::unicode_home;

use super::super::WindowsHost;
use super::{open_below, open_directory, read_through};

fn open(root: &Path, rel: &str) -> Entry {
    WindowsHost
        .open_entry(root, rel)
        .unwrap_or_else(|e| panic!("open_entry({rel}): {e}"))
}

#[skuld::test]
fn a_junction_above_the_entry_is_named_and_not_followed(#[fixture(unicode_home)] home: &Path) {
    let outside = home.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("f.txt"), b"not in the root").unwrap();
    let root = home.join("root");
    std::fs::create_dir_all(root.join("a")).unwrap();
    junction::create(&outside, root.join("a").join("b")).unwrap();
    for rel in ["a/b/f.txt", "a/b/gone", "a/b/deeper/f.txt"] {
        match open(&root, rel) {
            Entry::BelowSymlink(found) => assert_eq!(found, "a/b", "{rel}"),
            other => panic!("{rel}: {other:?}"),
        }
    }
}

#[skuld::test]
fn a_junction_at_the_end_is_a_symlink(#[fixture(unicode_home)] home: &Path) {
    let target = home.join("target");
    std::fs::create_dir(&target).unwrap();
    junction::create(&target, home.join("junction")).unwrap();
    assert!(matches!(open(home, "junction"), Entry::Symlink));
}

#[skuld::test]
fn a_name_is_found_whatever_its_case(#[fixture(unicode_home)] home: &Path) {
    std::fs::create_dir(home.join("Dir")).unwrap();
    std::fs::write(home.join("Dir").join("File.TXT"), b"x").unwrap();
    assert!(matches!(open(home, "dIR/file.txt"), Entry::File(_)));
}

#[skuld::test]
fn a_backslash_is_not_a_separator_in_a_name(#[fixture(unicode_home)] home: &Path) {
    std::fs::create_dir(home.join("a")).unwrap();
    std::fs::write(home.join("a").join("f.txt"), b"x").unwrap();
    let error = WindowsHost.open_entry(home, r"a\f.txt").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error}");
}

#[skuld::test]
fn a_name_windows_cannot_hold_is_absent(#[fixture(unicode_home)] home: &Path) {
    assert!(matches!(open(home, "a|b"), Entry::Absent));
}

/// Makes `path` (an existing file) a reparse point with a `tag` that is not a name surrogate and that nothing on this
/// machine handles, so a plain open of it fails. NTFS accepts the Microsoft range (bit 31) here without a privilege;
/// it refuses a third-party tag such as 0x1234.
fn make_unhandled_reparse_point(path: &Path, tag: u32) {
    // REPARSE_GUID_DATA_BUFFER: tag, data length, reserved, GUID, then the data (none here).
    let mut buffer = Vec::new();
    buffer.extend_from_slice(&tag.to_le_bytes());
    buffer.extend_from_slice(&16u16.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&[0x5a; 16]);
    let file = OpenOptions::new().write(true).open(path).unwrap();
    let mut returned = 0u32;
    // SAFETY: `buffer` holds a complete REPARSE_GUID_DATA_BUFFER of the length passed, `file` is open for writing, and
    // there is no output buffer.
    let set = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as HANDLE,
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(set, 0, "FSCTL_SET_REPARSE_POINT: {}", std::io::Error::last_os_error());
}

/// `d\a:b` reads the stream `b` of the file `d\a`, which is no file git tracks.
#[skuld::test]
fn a_name_with_a_colon_is_absent_and_a_stream_is_never_read(#[fixture(unicode_home)] home: &Path) {
    std::fs::create_dir(home.join("d")).unwrap();
    std::fs::write(home.join("d").join("a"), b"the file").unwrap();
    // NTFS reads `:` as a stream separator whatever the prefix; the `\\?\` prefix is dropped to leave an ordinary path.
    let text = home.to_str().expect("the test directory is Unicode");
    let stream = format!(r"{}\d\a:b", text.strip_prefix(r"\\?\").unwrap_or(text));
    std::fs::write(&stream, b"the stream").unwrap();
    assert_eq!(
        std::fs::read(&stream).unwrap(),
        b"the stream",
        "there is a stream to be read"
    );
    for rel in ["d/a:b", "d/a:b/below", "d/a::$DATA", "a:b"] {
        assert!(matches!(open(home, rel), Entry::Absent), "{rel}");
    }
}

#[skuld::test]
fn a_reparse_point_that_is_not_a_link_is_read_through_its_filter(#[fixture(unicode_home)] home: &Path) {
    let file = home.join("placeholder.txt");
    std::fs::write(&file, b"x").unwrap();
    make_unhandled_reparse_point(&file, 0x8000_1234);
    // Opened as itself it is a file; opened through its filter, which does not exist here, it is not readable.
    let result = WindowsHost.open_entry(home, "placeholder.txt");
    std::fs::remove_file(&file).unwrap();
    let error = result.expect_err("a reparse point nothing handles cannot be read through");
    assert_ne!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
}

#[skuld::test]
fn the_name_reopened_through_its_filter_must_be_the_file_that_was_opened(#[fixture(unicode_home)] home: &Path) {
    std::fs::write(home.join("x"), b"x").unwrap();
    std::fs::write(home.join("y"), b"y").unwrap();
    let directory = open_directory(home).unwrap();
    let first = open_below(&directory, "x", true, FILE_GENERIC_READ).unwrap();
    read_through(&directory, "x", &first, &["x"], FILE_GENERIC_READ).expect("the same file again");
    let error = read_through(&directory, "y", &first, &["d", "y"], FILE_GENERIC_READ).unwrap_err();
    assert!(error.to_string().contains("`d/y`"), "{error}");
}

/// Denies everyone `FILE_LIST_DIRECTORY` on a directory for as long as it lives, then puts the old DACL back, so the
/// directory can be deleted. The user may still traverse it.
struct ListDenied {
    path: Vec<u16>,
    old_acl: *mut ACL,
    descriptor: PSECURITY_DESCRIPTOR,
}

impl ListDenied {
    fn new(directory: &Path) -> Self {
        let path: Vec<u16> = directory.as_os_str().encode_wide().chain([0]).collect();
        let mut old_acl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `path` is NUL-terminated; the out-pointers are valid and the unwanted ones are null.
        let status = unsafe {
            GetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut old_acl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(
            status,
            0,
            "GetNamedSecurityInfoW: {}",
            std::io::Error::from_raw_os_error(status as i32)
        );
        // `WD` is Everyone in every language.
        let sid_text: Vec<u16> = "WD".encode_utf16().chain([0]).collect();
        let mut everyone: PSID = std::ptr::null_mut();
        // SAFETY: `sid_text` is NUL-terminated and `everyone` is a valid out-pointer.
        let converted = unsafe { ConvertStringSidToSidW(sid_text.as_ptr(), &mut everyone) };
        assert_ne!(
            converted,
            0,
            "ConvertStringSidToSidW: {}",
            std::io::Error::last_os_error()
        );
        let mut entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_LIST_DIRECTORY,
            grfAccessMode: DENY_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: Default::default(),
        };
        entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
        entry.Trustee.TrusteeType = TRUSTEE_IS_WELL_KNOWN_GROUP;
        entry.Trustee.ptstrName = everyone.cast();
        let mut new_acl: *mut ACL = std::ptr::null_mut();
        // SAFETY: `entry` names a SID that lives until the LocalFree below; `old_acl` is the DACL of `descriptor`.
        let status = unsafe { SetEntriesInAclW(1, &entry, old_acl, &mut new_acl) };
        assert_eq!(
            status,
            0,
            "SetEntriesInAclW: {}",
            std::io::Error::from_raw_os_error(status as i32)
        );
        // SAFETY: `path` is NUL-terminated and `new_acl` is a complete ACL.
        let status = unsafe {
            SetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                new_acl,
                std::ptr::null(),
            )
        };
        // SAFETY: both were allocated by the calls above and nothing uses them after this.
        unsafe {
            LocalFree(new_acl.cast());
            LocalFree(everyone);
        }
        assert_eq!(
            status,
            0,
            "SetNamedSecurityInfoW: {}",
            std::io::Error::from_raw_os_error(status as i32)
        );
        Self {
            path,
            old_acl,
            descriptor,
        }
    }
}

impl Drop for ListDenied {
    fn drop(&mut self) {
        // SAFETY: `path` is NUL-terminated, and `old_acl` lives in `descriptor`, which is freed after the call.
        let status = unsafe {
            let status = SetNamedSecurityInfoW(
                self.path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                self.old_acl,
                std::ptr::null(),
            );
            LocalFree(self.descriptor);
            status
        };
        // A second panic while unwinding would abort and hide the first.
        if status != 0 && !std::thread::panicking() {
            panic!(
                "restoring the DACL: {}",
                std::io::Error::from_raw_os_error(status as i32)
            );
        }
    }
}

fn read_all(mut file: std::fs::File) -> Vec<u8> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    bytes
}

/// The user may traverse a directory they cannot list, and a path through it works.
#[skuld::test]
fn a_directory_that_can_be_traversed_but_not_listed_is_walked(#[fixture(unicode_home)] home: &Path) {
    let root = home.join("root");
    std::fs::create_dir_all(root.join("d")).unwrap();
    std::fs::write(root.join("d").join("f.txt"), b"inside").unwrap();
    let denied = ListDenied::new(&root.join("d"));
    assert!(
        std::fs::read_dir(root.join("d")).is_err(),
        "the directory is not listable"
    );
    let found = WindowsHost.open_entry(&root, "d/f.txt");
    drop(denied);
    match found.unwrap_or_else(|e| panic!("open_entry: {e}")) {
        Entry::File(file) => assert_eq!(read_all(file), b"inside"),
        other => panic!("{other:?}"),
    }
}

#[skuld::test]
fn a_root_that_can_be_traversed_but_not_listed_is_walked(#[fixture(unicode_home)] home: &Path) {
    let root = home.join("root");
    std::fs::create_dir_all(root.join("d")).unwrap();
    std::fs::write(root.join("d").join("f.txt"), b"inside").unwrap();
    let denied = ListDenied::new(&root);
    assert!(std::fs::read_dir(&root).is_err(), "the root is not listable");
    let found = WindowsHost.open_entry(&root, "d/f.txt");
    drop(denied);
    match found.unwrap_or_else(|e| panic!("open_entry: {e}")) {
        Entry::File(file) => assert_eq!(read_all(file), b"inside"),
        other => panic!("{other:?}"),
    }
}

/// A directory at the end of the path is reported as one, listable or not.
#[skuld::test]
fn a_directory_that_cannot_be_listed_is_a_directory(#[fixture(unicode_home)] home: &Path) {
    std::fs::create_dir(home.join("d")).unwrap();
    let denied = ListDenied::new(&home.join("d"));
    let found = WindowsHost.open_entry(home, "d");
    drop(denied);
    assert!(matches!(
        found.unwrap_or_else(|e| panic!("open_entry: {e}")),
        Entry::Directory
    ));
}
