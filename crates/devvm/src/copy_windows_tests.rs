use std::path::Path;

use crate::ErrorKind;
use crate::copy::copy_tests::{add_all, new_repo, put, snapshot_through_a_directory_link, take};
use crate::test_support::fixtures::unicode_home;

/// Git for Windows resolves the directory it starts in, and Win32 collapses `..` in a path without resolving it, so a
/// root found as the start plus `../..` is another directory when the start is a junction.
#[skuld::test]
fn a_from_dir_reached_through_a_junction_snapshots_the_repository(#[fixture(unicode_home)] home: &Path) {
    snapshot_through_a_directory_link(home, |target, link| junction::create(target, link).unwrap());
}

#[skuld::test]
fn a_tracked_path_below_a_junction_is_refused_naming_it(#[fixture(unicode_home)] home: &Path) {
    let outside = home.join("outside");
    put(&outside, "f.txt", b"a host file that is not in the repository");
    let repo = new_repo(home, "repo");
    put(&repo, "dir/f.txt", b"x");
    add_all(&repo);
    std::fs::remove_dir_all(repo.join("dir")).unwrap();
    junction::create(&outside, repo.join("dir")).unwrap();
    let error = take(&repo, false, home)
        .err()
        .expect("a file outside the repository must not be copied");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("dir/f.txt"), "{error}");
}
