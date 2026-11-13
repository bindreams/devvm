//! `extract_command`'s script in a real PowerShell 7 on the test host (the `pwsh_wrap` group: it spawns `pwsh`, which the
//! CI Windows Test lane and the deputy's runner do). The archives are the snapshot's shape: a tar of files and links, no
//! directory entries.

use std::path::Path;
use std::process::Output;

use super::extract_command;
use crate::copy::copy_tests::{add_all, new_repo, put, take};
use crate::guest::windows::wrap::wrap_pwsh_tests::{run, shown};
use crate::test_groups::{Group, pwsh_wrap};
use crate::test_support::fixtures::unicode_home;

/// `path` as the user types a guest path: with `/`.
fn typed(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// A tar of the files `entries` (name, content), written to `archive`.
fn write_tar(archive: &Path, entries: &[(&str, &[u8])]) {
    let mut builder = tar::Builder::new(std::fs::File::create(archive).unwrap());
    for (name, content) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        builder.append_data(&mut header, name, *content).unwrap();
    }
    builder.finish().unwrap();
}

/// Runs the guest's extraction of `archive` into `destination` in the host's `pwsh`, as the guest's broker would.
fn extract(destination: &Path, archive: &Path) -> Output {
    let command = extract_command(&typed(destination), &typed(archive)).unwrap();
    run(command.argv.last().unwrap(), "none", None)
}

#[skuld::test]
fn an_archive_the_snapshot_makes_extracts_with_the_windows_script(
    #[fixture(pwsh_wrap)] _group: &Group,
    #[fixture(unicode_home)] home: &Path,
) {
    let long_directory = ["a-directory-of-a-long-name"; 12].join("/");
    let long_file = format!("{long_directory}/deep.txt");
    assert!(home.join(&long_file).as_os_str().len() > 260, "a path past MAX_PATH");
    let archive = home.join("devvm-copy-x.tar");
    let entries: [(&str, &[u8]); 6] = [
        ("a.txt", b"new"),
        ("dir with space/b.txt", b"b"),
        ("\u{e9} \u{416} \u{4e16}/\u{1f600}.txt", b"unicode"),
        (&long_file, b"deep"),
        ("empty.txt", b""),
        ("it's $x `y` [1].txt", b"quoted"),
    ];
    write_tar(&archive, &entries);
    // The destination does not exist, and holds a file the archive overwrites once it is made.
    let destination = home.join("made \u{1f600} here").join("sub");
    std::fs::create_dir_all(&destination).unwrap();
    std::fs::write(destination.join("a.txt"), "old, and longer than the new one").unwrap();
    std::fs::remove_file(destination.join("a.txt")).unwrap();
    std::fs::write(destination.join("a.txt"), "old, and longer than the new one").unwrap();
    let missing = home.join("missing \u{1f600}").join("deeper").join("still");

    let overwritten = extract(&destination, &archive);
    let made = {
        write_tar(&archive, &entries);
        extract(&missing, &archive)
    };

    for (finished, into) in [(&overwritten, &destination), (&made, &missing)] {
        assert_eq!(finished.status.code(), Some(0), "{}", shown(finished));
        for (name, content) in entries {
            assert_eq!(std::fs::read(into.join(name)).unwrap(), content, "{name}");
        }
    }
    assert!(!archive.exists(), "the archive is removed");
}

#[skuld::test]
fn an_empty_snapshot_makes_the_destination_and_removes_the_archive(
    #[fixture(pwsh_wrap)] _group: &Group,
    #[fixture(unicode_home)] home: &Path,
) {
    let archive = home.join("devvm-copy-x.tar");
    write_tar(&archive, &[]);
    let destination = home.join("empty one");

    let finished = extract(&destination, &archive);

    assert_eq!(finished.status.code(), Some(0), "{}", shown(&finished));
    assert!(destination.is_dir());
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
    assert!(!archive.exists());
}

#[skuld::test]
fn the_archive_is_gone_afterwards_also_when_the_extraction_fails(
    #[fixture(pwsh_wrap)] _group: &Group,
    #[fixture(unicode_home)] home: &Path,
) {
    let archive = home.join("devvm-copy-x.tar");
    std::fs::write(&archive, vec![0xA5u8; 4096]).unwrap();
    let destination = home.join("never filled");

    let finished = extract(&destination, &archive);

    assert_ne!(finished.status.code(), Some(0), "{}", shown(&finished));
    assert!(
        String::from_utf8_lossy(&finished.stderr).contains("devvm: cannot extract"),
        "{}",
        shown(&finished)
    );
    assert!(!archive.exists(), "the archive is removed whatever the extraction did");
}

#[skuld::test]
fn a_snapshot_entry_the_windows_extraction_cannot_make_ends_the_step_naming_it_and_is_never_skipped(
    #[fixture(pwsh_wrap)] _group: &Group,
    #[fixture(unicode_home)] home: &Path,
) {
    // The snapshot keeps a tracked symlink as a symlink entry. A token that may not make one (the guest user at Medium
    // integrity) fails the extraction; a host that may makes the link. Either way the entry is neither skipped nor
    // quietly turned into something else.
    let archive = home.join("devvm-copy-x.tar");
    let mut builder = tar::Builder::new(std::fs::File::create(&archive).unwrap());
    let mut file = tar::Header::new_gnu();
    file.set_size(1);
    file.set_mode(0o644);
    builder.append_data(&mut file, "target.txt", &b"t"[..]).unwrap();
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    link.set_mode(0o777);
    builder.append_link(&mut link, "a link.txt", "target.txt").unwrap();
    builder.finish().unwrap();
    drop(builder);
    let destination = home.join("with a link");

    let finished = extract(&destination, &archive);

    let link = destination.join("a link.txt");
    if finished.status.code() == Some(0) {
        assert!(
            std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
            "the extraction succeeded without making the link: {}",
            shown(&finished)
        );
    } else {
        let stderr = String::from_utf8_lossy(&finished.stderr);
        assert!(stderr.contains("a link.txt"), "{}", shown(&finished));
    }
    assert!(!archive.exists());
}

/// Whether the file has the ReadOnly attribute, which is what Rust's `readonly` reads on Windows.
fn read_only(path: &Path) -> bool {
    std::fs::metadata(path).unwrap().permissions().readonly()
}

/// A snapshot of `repo` made as a `copy` step makes it, copied to `copy` so that a second extraction can use it too:
/// the extraction removes the archive it is given.
fn snapshot_of(repo: &Path, read_only: bool, scratch: &Path) -> std::path::PathBuf {
    let archive = take(repo, read_only, scratch).unwrap().snapshot.archive;
    let copy = scratch.join("copy.tar");
    std::fs::copy(&archive, &copy).unwrap();
    std::fs::remove_file(&archive).unwrap();
    copy
}

#[skuld::test]
fn a_read_only_snapshot_makes_read_only_files_in_writable_directories_and_a_second_copy_overwrites_them(
    #[fixture(pwsh_wrap)] _group: &Group,
    #[fixture(unicode_home)] home: &Path,
) {
    let repo = new_repo(home, "repo");
    put(&repo, "a.txt", b"first");
    put(&repo, "dir/b.txt", b"b");
    add_all(&repo);
    let destination = home.join("target");

    let first = extract(&destination, &snapshot_of(&repo, true, &home.join("one")));
    put(&repo, "a.txt", b"second, and longer than the first");
    add_all(&repo);
    let second = extract(&destination, &snapshot_of(&repo, true, &home.join("two")));

    assert_eq!(first.status.code(), Some(0), "{}", shown(&first));
    assert_eq!(second.status.code(), Some(0), "{}", shown(&second));
    assert_eq!(
        std::fs::read_to_string(destination.join("a.txt")).unwrap(),
        "second, and longer than the first",
        "a read-only file from the earlier copy is overwritten"
    );
    for file in ["a.txt", "dir/b.txt"] {
        assert!(read_only(&destination.join(file)), "{file}");
    }
    assert!(!read_only(&destination.join("dir")), "directories stay writable");
    assert!(!read_only(&destination));
}

#[skuld::test]
fn a_snapshot_that_is_not_read_only_leaves_no_file_read_only_and_overwrites_one_that_is(
    #[fixture(pwsh_wrap)] _group: &Group,
    #[fixture(unicode_home)] home: &Path,
) {
    let repo = new_repo(home, "repo");
    put(&repo, "a.txt", b"new");
    put(&repo, "b.txt", b"b");
    add_all(&repo);
    let destination = home.join("target");
    std::fs::create_dir_all(&destination).unwrap();
    // A file a `run` step made read-only, as `attrib +r` does.
    let earlier = destination.join("a.txt");
    std::fs::write(&earlier, "old, and longer than the new one").unwrap();
    let mut permissions = std::fs::metadata(&earlier).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&earlier, permissions).unwrap();

    let finished = extract(&destination, &snapshot_of(&repo, false, home));

    assert_eq!(finished.status.code(), Some(0), "{}", shown(&finished));
    assert_eq!(std::fs::read_to_string(&earlier).unwrap(), "new");
    for file in ["a.txt", "b.txt"] {
        assert!(!read_only(&destination.join(file)), "{file}");
    }
}
