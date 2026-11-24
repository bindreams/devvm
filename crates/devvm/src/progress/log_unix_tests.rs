//! The ways a kept copy fails that need file modes, or a directory read as a file.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::log_tests::copy_of;
use crate::test_support::fixtures::unicode_home;

/// The files in `dir`, which must exist.
fn files_in(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

/// A directory opens for reading on Unix and fails at the first read.
#[skuld::test]
fn a_copy_that_fails_half_way_is_removed_and_the_error_names_both_ends(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("logs");
    let source = home.join("a directory");
    std::fs::create_dir(&source).unwrap();
    let mut warnings = Vec::new();

    let error = copy_of(&dir, &source, &mut warnings).unwrap_err();

    let message = error.to_string();
    assert!(
        message.starts_with(&format!("cannot copy {} to ", source.display())),
        "{message}"
    );
    assert!(message.contains(".serial.log"), "{message}");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(files_in(&dir), Vec::<String>::new(), "no partial copy is left");
}

/// Mode bits do not bind root, so the test refuses to run as root.
#[skuld::test]
fn a_directory_the_copy_cannot_be_made_in_is_an_error_naming_the_file(#[fixture(unicode_home)] home: &Path) {
    // SAFETY: `geteuid` reads a process attribute.
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "directory modes do not bind root: run this test as another user"
    );
    let dir = home.join("logs");
    std::fs::create_dir(&dir).unwrap();
    let source = home.join("serial.log");
    std::fs::write(&source, b"x").unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();

    let copied = copy_of(&dir, &source, &mut Vec::new());

    // The temporary directory cannot be removed with the mode left as it is.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let error = copied.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied, "{error}");
    let message = error.to_string();
    assert!(
        message.starts_with(&format!("cannot write {}", dir.join("devvm-").display())),
        "{message}"
    );
    assert_eq!(files_in(&dir), Vec::<String>::new());
}
