use std::fs::OpenOptions;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::HANDLE;
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
    // Win32 reads `:` as a stream separator except in a `\?\` path, which takes every character as a name.
    let text = home.to_str().expect("the test directory is Unicode");
    let stream = format!(r"{}\d\a:b", text.strip_prefix(r"\?\").unwrap_or(text));
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
    let first = open_below(&directory, "x", true).unwrap();
    read_through(&directory, "x", &first, &["x"]).expect("the same file again");
    let error = read_through(&directory, "y", &first, &["d", "y"]).unwrap_err();
    assert!(error.to_string().contains("`d/y`"), "{error}");
}
