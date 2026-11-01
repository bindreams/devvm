use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::ErrorKind;
use crate::copy::copy_tests::{
    add_all, git, git_command, new_repo, paths, put, read_tar, snapshot_through_a_directory_link, take,
};
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
    let status = git_command(&repo)
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

#[skuld::test]
fn a_tracked_symlink_that_is_a_regular_file_now_is_refused_naming_it(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "real.conf", b"r");
    std::os::unix::fs::symlink("real.conf", repo.join("config")).unwrap();
    add_all(&repo);
    // The default `core.symlinks` here is true: a regular file where a link is tracked is a typechange.
    std::fs::remove_file(repo.join("config")).unwrap();
    put(&repo, "config", b"content of my own");
    let error = take(&repo, false, home)
        .err()
        .expect("a file the user wrote is not the tracked link");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("`config`"), "{error}");
    assert!(error.message().contains("git tracks a symlink"), "{error}");
    assert!(error.message().contains("regular file"), "{error}");
}

#[skuld::test]
fn a_from_dir_reached_through_a_symlink_snapshots_the_repository(#[fixture(unicode_home)] home: &Path) {
    snapshot_through_a_directory_link(home, |target, link| std::os::unix::fs::symlink(target, link).unwrap());
}

/// Makes a FIFO at `path`.
fn make_fifo(path: &Path) {
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path without NUL");
    // SAFETY: `name` is a NUL-terminated path that outlives the call.
    let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(made, 0, "mkfifo: {}", std::io::Error::last_os_error());
}

#[skuld::test]
fn a_tracked_file_that_is_a_fifo_or_a_socket_now_is_refused_naming_it(#[fixture(unicode_home)] home: &Path) {
    for (i, (what, fifo)) in [("a FIFO", true), ("a socket", false)].into_iter().enumerate() {
        let repo = new_repo(home, &format!("repo{i}"));
        put(&repo, "thing", b"x");
        add_all(&repo);
        std::fs::remove_file(repo.join("thing")).unwrap();
        // The socket file stays while the listener lives.
        let _listener = if fifo {
            make_fifo(&repo.join("thing"));
            None
        } else {
            Some(std::os::unix::net::UnixListener::bind(repo.join("thing")).expect("a socket"))
        };
        let error = take(&repo, false, &home.join(i.to_string()))
            .err()
            .unwrap_or_else(|| panic!("{what} is not the tracked file"));
        assert_eq!(error.kind(), ErrorKind::Recipe, "{what}: {error}");
        assert!(error.message().contains("`thing`"), "{what}: {error}");
        assert!(
            error.message().contains("neither a file nor a directory"),
            "{what}: {error}"
        );
    }
}

#[skuld::test]
fn a_tracked_symlink_that_is_a_fifo_now_is_refused_naming_it(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    std::os::unix::fs::symlink("elsewhere", repo.join("config")).unwrap();
    add_all(&repo);
    std::fs::remove_file(repo.join("config")).unwrap();
    make_fifo(&repo.join("config"));
    let error = take(&repo, false, home).err().expect("a FIFO is not the tracked link");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("`config`"), "{error}");
    assert!(error.message().contains("neither a link nor a file"), "{error}");
}

/// Mode bits do not bind root, so the test refuses to run as root.
#[skuld::test]
fn a_tracked_file_that_cannot_be_read_is_an_error_naming_it(#[fixture(unicode_home)] home: &Path) {
    // SAFETY: `geteuid` reads a process attribute.
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "file modes do not bind root: run this test as another user"
    );
    let repo = new_repo(home, "repo");
    put(&repo, "secret.txt", b"x");
    add_all(&repo);
    let path = repo.join("secret.txt");
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    let taken = take(&repo, false, home);
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let error = taken.err().expect("a file that cannot be read is not skipped");
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains("secret.txt"), "{error}");
}
