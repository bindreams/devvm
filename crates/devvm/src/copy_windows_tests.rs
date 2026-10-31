use std::path::Path;

use crate::copy::copy_tests::snapshot_through_a_directory_link;
use crate::test_support::fixtures::unicode_home;

/// Git for Windows resolves the directory it starts in, and Win32 collapses `..` in a path without resolving it, so a
/// root found as the start plus `../..` is another directory when the start is a junction.
#[skuld::test]
fn a_from_dir_reached_through_a_junction_snapshots_the_repository(#[fixture(unicode_home)] home: &Path) {
    snapshot_through_a_directory_link(home, |target, link| junction::create(target, link).unwrap());
}
