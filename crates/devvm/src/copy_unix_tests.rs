use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::ErrorKind;
use crate::copy::copy_tests::{add_all, git, new_repo, put, read_tar, take};
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
