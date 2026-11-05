use std::path::Path;

use super::FileLock;
use crate::test_support::fixtures::unicode_home;

/// The open fails with "no such file" because the link leads into a directory that is not there; the lock file's own
/// directory is, so the error does not say it is missing.
#[skuld::test]
fn a_dangling_symlink_at_the_lock_path_gives_the_plain_open_error(#[fixture(unicode_home)] home: &Path) {
    let link = home.join("a.lock");
    std::os::unix::fs::symlink(home.join("missing").join("b.lock"), &link).unwrap();

    let error = FileLock::open_in_existing_dir(&link).unwrap_err();

    assert!(error.message().contains(&link.display().to_string()), "{error}");
    assert!(!error.message().contains("does not exist"), "{error}");
}
