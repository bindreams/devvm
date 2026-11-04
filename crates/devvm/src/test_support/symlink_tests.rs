use std::path::Path;

use super::{failure, symlink};
use crate::test_groups::{Group, symlinks};
use crate::test_support::fixtures::unicode_home;

/// A relative target is read from the directory of the link, as the file system reads it, not from the current
/// directory: on Windows the kind of link depends on what the target is.
#[skuld::test]
fn a_relative_target_is_looked_up_beside_the_link(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(symlinks)] _group: &Group,
) {
    std::fs::create_dir(home.join("dir")).unwrap();
    std::fs::write(home.join("file"), b"x").unwrap();
    symlink(Path::new("dir"), &home.join("to-dir"));
    symlink(Path::new("file"), &home.join("to-file"));
    assert_eq!(std::fs::read_dir(home.join("to-dir")).unwrap().count(), 0);
    assert_eq!(std::fs::read(home.join("to-file")).unwrap(), b"x");
}

/// Only the error a host without the right gives is blamed on the right and answered with the opt-out.
#[skuld::test]
fn a_failure_for_the_missing_right_names_the_opt_out() {
    let message = failure(Path::new("t"), Path::new("l"), &std::io::Error::other("denied"), true);

    assert!(message.contains(symlinks::OPT_OUT), "{message}");
    assert!(message.contains("denied"), "{message}");
}

#[skuld::test]
fn any_other_failure_is_the_plain_error() {
    let message = failure(Path::new("t"), Path::new("l"), &std::io::Error::other("exists"), false);

    assert!(!message.contains(symlinks::OPT_OUT), "{message}");
    assert!(!message.contains("Developer Mode"), "{message}");
    assert!(message.contains("exists"), "{message}");
}
