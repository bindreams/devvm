use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::ErrorKind;
use crate::copy::copy_tests::{add_all, git, new_repo, paths, put, read_tar, take};
use crate::recipe::hash::EntryContent;
use crate::test_support::fixtures::unicode_home;

#[skuld::test]
fn a_real_symlink_becomes_a_symlink_entry_and_is_never_followed(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "real.txt", b"r");
    std::os::unix::fs::symlink("real.txt", repo.join("link")).unwrap();
    // Dangling and pointing outside the repository: still just link text.
    std::os::unix::fs::symlink("../outside", repo.join("away")).unwrap();
    add_all(&repo);
    let taken = take(&repo, false, home).unwrap();
    let targets: Vec<_> = taken
        .snapshot
        .manifest
        .entries()
        .iter()
        .map(|e| (e.path.as_str(), e.mode, e.content.clone()))
        .collect();
    assert_eq!(
        targets[..2],
        [
            ("away", 0o120000, EntryContent::SymlinkTarget("../outside".into())),
            ("link", 0o120000, EntryContent::SymlinkTarget("real.txt".into())),
        ]
    );
    let entries = read_tar(&taken.snapshot.archive);
    assert_eq!(entries[1].kind, tar::EntryType::Symlink);
    assert_eq!(entries[1].link.as_deref(), Some("real.txt"));
}

#[skuld::test]
fn non_utf8_names_are_refused_naming_them(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    // Index only: some file systems cannot hold the name, and the listing is refused before any file is read.
    let empty = git(&repo, &["hash-object", "-w", "--stdin"]);
    let mut entry = OsString::from(format!("100644,{},", empty.trim()));
    entry.push(OsStr::from_bytes(b"bad-\xff-name"));
    let status = std::process::Command::new("git")
        .current_dir(&repo)
        .args(["update-index", "--add", "--cacheinfo"])
        .arg(entry)
        .status()
        .expect("the tests need `git` on PATH");
    assert!(status.success());
    let error = take(&repo, false, home)
        .err()
        .expect("a non-UTF-8 name cannot be copied");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("bad-"), "{error}");
}

#[skuld::test]
fn names_with_characters_windows_forbids(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    let names = [
        "\"quoted\".txt",
        "a|b",
        "what?*",
        "<angle>",
        "colon:name",
        "back\\slash",
        "line\nbreak",
    ];
    for (i, name) in names.iter().enumerate() {
        put(&repo, name, format!("content {i}").as_bytes());
    }
    add_all(&repo);
    let taken = take(&repo, false, home).unwrap();
    let mut expected: Vec<&str> = names.to_vec();
    expected.sort_by_key(|name| name.as_bytes());
    assert_eq!(paths(&taken.snapshot), expected);
    for entry in read_tar(&taken.snapshot.archive) {
        let i = names.iter().position(|name| *name == entry.path).unwrap();
        assert_eq!(entry.bytes, format!("content {i}").as_bytes(), "{}", entry.path);
    }
}

#[skuld::test]
fn a_tracked_path_below_a_symlink_leaving_the_repository_is_refused_naming_it(#[fixture(unicode_home)] home: &Path) {
    let outside = home.join("outside");
    put(&outside, "f.txt", b"a host file that is not in the repository");
    let repo = new_repo(home, "repo");
    put(&repo, "dir/f.txt", b"x");
    add_all(&repo);
    std::fs::remove_dir_all(repo.join("dir")).unwrap();
    std::os::unix::fs::symlink(&outside, repo.join("dir")).unwrap();
    let error = take(&repo, false, home)
        .err()
        .expect("a file outside the repository must not be copied");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("dir/f.txt"), "{error}");
}

#[skuld::test]
fn a_tracked_path_below_a_deeper_symlink_is_refused_even_when_it_stays_inside(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "real/f.txt", b"x");
    put(&repo, "a/b/f.txt", b"y");
    add_all(&repo);
    std::fs::remove_dir_all(repo.join("a").join("b")).unwrap();
    std::os::unix::fs::symlink(repo.join("real"), repo.join("a").join("b")).unwrap();
    let error = take(&repo, false, home)
        .err()
        .expect("git refuses to go beyond a symbolic link, and so does the copy");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("a/b/f.txt"), "{error}");
}

#[skuld::test]
fn a_tracked_file_that_is_now_a_symlink_is_refused_naming_it(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "thing", b"x");
    put(&repo, "other", b"y");
    add_all(&repo);
    std::fs::remove_file(repo.join("thing")).unwrap();
    std::os::unix::fs::symlink("other", repo.join("thing")).unwrap();
    let error = take(&repo, false, home)
        .err()
        .expect("a symlink is not the tracked file");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("`thing`"), "{error}");
    assert!(error.message().contains("symbolic link"), "{error}");
    assert!(error.message().contains("git tracks a file"), "{error}");
    let left: Vec<_> = std::fs::read_dir(home.join("out")).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
}

#[skuld::test]
fn the_path_of_a_skipped_submodule_is_escaped_in_the_note(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "top.txt", b"top");
    add_all(&repo);
    // A gitlink with no directory on disk, as an uninitialised submodule leaves it.
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,1111111111111111111111111111111111111111,line\nbreak",
        ],
    );
    let taken = take(&repo, false, home).unwrap();
    assert_eq!(taken.notes.len(), 1, "{:?}", taken.notes);
    assert!(!taken.notes[0].contains('\n'), "{:?}", taken.notes);
    assert!(taken.notes[0].contains(r"line\nbreak"), "{:?}", taken.notes);
}

#[skuld::test]
fn a_gitlink_path_that_is_a_regular_file_now_is_skipped_as_uninitialised(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "top.txt", b"top");
    add_all(&repo);
    // The gitlink is in the index only; on disk its path holds a file, so `sub/.git` is "not a directory" to the OS.
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,1111111111111111111111111111111111111111,sub",
        ],
    );
    put(&repo, "sub", b"a file where the submodule was");
    let taken = take(&repo, false, home).expect("no work tree in `sub`, so the gitlink is skipped");
    assert_eq!(paths(&taken.snapshot), ["top.txt"]);
    assert_eq!(taken.notes.len(), 1, "{:?}", taken.notes);
    assert!(taken.notes[0].contains("`sub`"), "{:?}", taken.notes);
}
