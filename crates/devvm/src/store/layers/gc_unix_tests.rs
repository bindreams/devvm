use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::ErrorKind;
use crate::test_support::fixtures::unicode_home;

use super::gc::present;

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A parent that cannot be searched makes the look fail; that says nothing about the directory. Mode bits do not bind
/// root, so the test refuses to run as root.
#[skuld::test]
fn a_generation_directory_that_cannot_be_looked_at_is_an_error_not_gone(#[fixture(unicode_home)] home: &Path) {
    // SAFETY: `geteuid` reads a process attribute.
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "directory modes do not bind root: run this test as another user"
    );
    let parent = home.join("locked");
    let dir = parent.join("gen");
    std::fs::create_dir_all(&dir).unwrap();
    set_mode(&parent, 0o000);
    let found = present(&dir);
    // The temporary directory cannot be removed with the mode left as it is.
    set_mode(&parent, 0o700);
    let error = found.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(
        error
            .message()
            .starts_with(&format!("cannot collect: cannot look at {}", dir.display())),
        "{error}"
    );
    assert!(error.message().ends_with("nothing was deleted"), "{error}");
}
