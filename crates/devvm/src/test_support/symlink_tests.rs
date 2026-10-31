use std::path::Path;

use super::symlink;
use crate::test_support::fixtures::unicode_home;

/// A relative target is read from the directory of the link, as the file system reads it, not from the current
/// directory: on Windows the kind of link depends on what the target is.
#[skuld::test]
fn a_relative_target_is_looked_up_beside_the_link(#[fixture(unicode_home)] home: &Path) {
    std::fs::create_dir(home.join("dir")).unwrap();
    std::fs::write(home.join("file"), b"x").unwrap();
    symlink(Path::new("dir"), &home.join("to-dir"));
    symlink(Path::new("file"), &home.join("to-file"));
    assert_eq!(std::fs::read_dir(home.join("to-dir")).unwrap().count(), 0);
    assert_eq!(std::fs::read(home.join("to-file")).unwrap(), b"x");
}
