use std::path::{Path, PathBuf};

use super::super::typed::Typed;
use super::{
    HostSide, finish_file, joined, lexically_normal, link_removal_failure, replace, without_trailing_separator,
};
use crate::access::sftp::copy::CopySide;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::host::{RecordingHost, TestHost};
use crate::test_support::symlink::symlink;

/// The text of `path` as written: `Path` equality ignores a trailing separator, so only the text can tell `C:\` from
/// `C:`.
fn text(path: &Path) -> String {
    path.as_os_str().to_string_lossy().into_owned()
}

/// A root is a directory: stripping its separator would name the drive's current directory (`C:`) or the volume device
/// (`\\?\C:`).
#[cfg(windows)]
#[skuld::test]
fn a_drive_root_keeps_its_separator() {
    for root in [r"C:\", r"D:\", "C:/", r"\\?\C:\", r"\\?\UNC\server\share\"] {
        let kept = without_trailing_separator(Path::new(root));
        assert_eq!(text(&kept), root, "{root}");
        assert!(kept.has_root(), "{root}");
    }
    // A UNC prefix has an implicit root, so the share without its separator is still the share's root.
    let share = without_trailing_separator(Path::new(r"\\server\share\"));
    assert_eq!(text(&share), r"\\server\share");
    assert!(share.has_root());
    assert_eq!(
        text(&without_trailing_separator(Path::new(r"C:\")).join("f.txt")),
        r"C:\f.txt"
    );
}

#[skuld::test]
fn a_trailing_separator_is_dropped_from_anything_but_a_root() {
    assert_eq!(text(&without_trailing_separator(Path::new("/"))), "/");
    assert_eq!(text(&without_trailing_separator(Path::new("a/b/"))), "a/b");
    assert_eq!(text(&without_trailing_separator(Path::new("a/b"))), "a/b");
    #[cfg(windows)]
    {
        assert_eq!(text(&without_trailing_separator(Path::new(r"C:\work\"))), r"C:\work");
        assert_eq!(text(&without_trailing_separator(Path::new("C:work/"))), "C:work");
    }
}

#[skuld::test]
fn dot_and_dotdot_segments_are_resolved_by_name_without_touching_the_disk() {
    let sep = std::path::MAIN_SEPARATOR;
    let at = |path: &str| path.replace('/', &sep.to_string());
    for (given, want) in [
        ("new/.", "new"),
        ("./new", "new"),
        ("a/./b", "a/b"),
        ("a/../b", "b"),
        ("a/b/../../c", "c"),
        ("a/..", "."),
        (".", "."),
        ("..", ".."),
        ("../a", "../a"),
        ("a/../../b", "../b"),
    ] {
        assert_eq!(text(&lexically_normal(Path::new(&at(given)))), at(want), "{given}");
    }
}

/// `a:b` is a name here, but `PathBuf::push` would read it as a drive.
#[cfg(windows)]
#[skuld::test]
fn a_name_that_looks_like_a_drive_stays_a_name() {
    for (given, want) in [
        (r"x\a:b", r"x\a:b"),
        (r"x\y\..\a:b", r"x\a:b"),
        (r"C:\x\a:b\.", r"C:\x\a:b"),
    ] {
        assert_eq!(text(&lexically_normal(Path::new(given))), want, "{given}");
    }
}

/// The prefixes of a destination are the path read piece by piece, whatever a name looks like: `PathBuf::push` would
/// read `c:b` as a drive and drop what came before it, and the destination check would then look at the wrong path.
#[cfg(windows)]
#[skuld::test]
fn the_prefixes_of_a_destination_keep_a_drive_like_name_a_name() {
    let side = HostSide::new(&TestHost, Typed::none());
    for (given, want) in [
        (r"out\c:b", vec![r"out", r"out\c:b"]),
        (r"out\c:b\y", vec![r"out", r"out\c:b", r"out\c:b\y"]),
        (r"C:\out\d:b", vec![r"C:", r"C:\", r"C:\out", r"C:\out\d:b"]),
        (
            r"\\?\C:\out\d:b",
            vec![r"\\?\C:", r"\\?\C:\", r"\\?\C:\out", r"\\?\C:\out\d:b"],
        ),
        (
            r"\\server\share\out\d:b",
            vec![
                r"\\server\share",
                r"\\server\share\",
                r"\\server\share\out",
                r"\\server\share\out\d:b",
            ],
        ),
        (r"..\c:b", vec![r"..", r"..\c:b"]),
        (r".\c:b", vec![r".", r".\c:b"]),
        (r"C:b", vec![r"C:", r"C:b"]),
    ] {
        let chain = side.chain(&PathBuf::from(given));
        let got: Vec<String> = chain.iter().map(|(_, path)| text(path)).collect();
        assert_eq!(got, want, "{given}");
    }
}

/// A name joined to a base is that name under the base, on every host: it is never read as a drive or a root.
#[skuld::test]
fn a_name_joined_to_a_base_stays_under_it() {
    let sep = std::path::MAIN_SEPARATOR;
    let at = |path: &str| path.replace('/', &sep.to_string());
    for (base, name, want) in [
        ("out", "f", "out/f"),
        ("out/", "f", "out/f"),
        ("", "f", "f"),
        (".", "f", "./f"),
        ("..", "f", "../f"),
        ("a/b", "c", "a/b/c"),
    ] {
        assert_eq!(
            text(&joined(Path::new(&at(base)), name.as_ref())),
            at(want),
            "{base} + {name}"
        );
    }
    let side = HostSide::new(&TestHost, Typed::none());
    assert_eq!(text(&side.join(&PathBuf::from(at("out")), "x")), at("out/x"));
}

/// The same on Windows, where a name can look like a drive or a root.
#[cfg(windows)]
#[skuld::test]
fn a_drive_like_name_joined_to_a_base_stays_under_it() {
    for (base, name, want) in [
        ("out", "c:b", r"out\c:b"),
        (r"out\", "c:b", r"out\c:b"),
        (r"C:\", "d:b", r"C:\d:b"),
        ("C:", "b", "C:b"),
        (r"\\?\C:\", "d:b", r"\\?\C:\d:b"),
        (r"\\server\share\", "d:b", r"\\server\share\d:b"),
        ("", "c:b", "c:b"),
    ] {
        assert_eq!(text(&joined(Path::new(base), name.as_ref())), want, "{base} + {name}");
    }
}

#[cfg(windows)]
#[skuld::test]
fn dotdot_stops_at_the_root_of_a_drive() {
    for (given, want) in [
        (r"C:\a\..\b", r"C:\b"),
        (r"C:\..", r"C:\"),
        (r"C:\a\.", r"C:\a"),
        (r"\\server\share\a\..\..", r"\\server\share\"),
    ] {
        assert_eq!(text(&lexically_normal(Path::new(given))), want, "{given}");
    }
}

#[cfg(unix)]
#[skuld::test]
fn dotdot_stops_at_the_root() {
    assert_eq!(text(&lexically_normal(Path::new("/.."))), "/");
    assert_eq!(text(&lexically_normal(Path::new("/a/../.."))), "/");
}

#[skuld::test]
fn a_link_that_could_not_be_removed_reports_the_first_failure_and_then_the_second() {
    let as_file = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
    let as_dir = std::io::Error::other("not a directory");
    let both = link_removal_failure(as_file, as_dir);
    assert_eq!(both.kind(), std::io::ErrorKind::PermissionDenied);
    let text = both.to_string();
    assert!(text.starts_with("denied"), "{text}");
    assert!(text.contains("not a directory"), "{text}");

    // The same failure twice is said once.
    let twice = link_removal_failure(std::io::Error::other("busy"), std::io::Error::other("busy"));
    assert_eq!(twice.to_string(), "busy");
}

#[skuld::test]
fn create_dir_says_whether_it_made_the_directory(#[fixture(unicode_home)] home: &Path) {
    let side = HostSide::new(&TestHost, Typed::none());
    let deep = home.join("a").join("b").join("c");
    assert!(side.create_dir(&deep).unwrap(), "made, with its parents");
    assert!(deep.is_dir());
    assert!(!side.create_dir(&deep).unwrap(), "it was already there");
    assert!(
        !side.create_dir(&deep.parent().unwrap().to_path_buf()).unwrap(),
        "so was its parent"
    );
}

/// Creating a Windows drive root fails with access denied, not already exists, so a file copied to `C:\` met an error.
#[skuld::test]
fn create_dir_on_an_existing_root_finds_it_there(#[fixture(unicode_home)] home: &Path) {
    let side = HostSide::new(&TestHost, Typed::none());
    let root = home.ancestors().last().unwrap().to_path_buf();
    assert!(root.parent().is_none(), "{} is a root", root.display());
    assert!(!side.create_dir(&root).unwrap(), "{}", root.display());
}

#[skuld::test]
fn create_dir_does_not_follow_a_link_that_is_in_the_way(#[fixture(unicode_home)] home: &Path) {
    let side = HostSide::new(&TestHost, Typed::none());
    let real = home.join("real");
    std::fs::create_dir(&real).unwrap();
    let link = home.join("link");
    symlink(&real, &link);
    assert!(
        !side.create_dir(&link).unwrap(),
        "something is there, so the copy made nothing"
    );
    assert!(
        std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
        "the link is untouched"
    );
}

#[skuld::test]
fn create_dir_refuses_a_file_in_the_way(#[fixture(unicode_home)] home: &Path) {
    let side = HostSide::new(&TestHost, Typed::none());
    let file = home.join("file");
    std::fs::write(&file, "x").unwrap();
    assert!(side.create_dir(&file).is_err());
    assert!(side.create_dir(&file.join("below")).is_err());
}

#[skuld::test]
fn the_execute_bit_is_set_on_the_temporary_before_it_is_moved_into_place(#[fixture(unicode_home)] home: &Path) {
    let host = RecordingHost::new();
    let (temp, target) = (home.join("partial"), home.join("tool"));
    std::fs::write(&temp, "new").unwrap();
    std::fs::write(&target, "old").unwrap();
    finish_file(&temp, &target, true, None, &HostSide::new(&host, Typed::none())).unwrap();
    assert_eq!(
        host.set_executable_calls(),
        [temp],
        "never on the file that already replaced the old one"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
}

/// A umask without the owner write bit, or an ACL that lets the user create but not write, leaves the downloaded file
/// unopenable for write.
#[skuld::test]
fn the_time_is_set_on_a_temporary_that_cannot_be_opened_for_write(#[fixture(unicode_home)] home: &Path) {
    let (temp, target) = (home.join("partial"), home.join("tool"));
    std::fs::write(&temp, "new").unwrap();
    let mut permissions = std::fs::metadata(&temp).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&temp, permissions).unwrap();
    finish_file(
        &temp,
        &target,
        false,
        Some(1_000_000_000),
        &HostSide::new(&TestHost, Typed::none()),
    )
    .unwrap();
    let modified = std::fs::metadata(&target).unwrap().modified().unwrap();
    assert_eq!(
        modified.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        1_000_000_000
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
}

#[skuld::test]
fn a_file_that_cannot_be_made_executable_never_replaces_the_target(#[fixture(unicode_home)] home: &Path) {
    let host = RecordingHost::new();
    host.fail_set_executable();
    let (temp, target) = (home.join("partial"), home.join("tool"));
    std::fs::write(&temp, "new").unwrap();
    std::fs::write(&target, "old").unwrap();
    let error = finish_file(&temp, &target, true, None, &HostSide::new(&host, Typed::none())).unwrap_err();
    assert!(error.message().contains("cannot make executable"), "{error}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
    assert!(!temp.exists(), "the temporary file is removed");
}

#[skuld::test]
fn a_move_whose_cleanup_fails_too_names_both(#[fixture(unicode_home)] home: &Path) {
    // A directory cannot be moved over a directory that holds something, and `remove_file` cannot remove it.
    let (temp, target) = (home.join("partial"), home.join("tool"));
    std::fs::create_dir(&temp).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("held"), "old").unwrap();
    let error = replace(&HostSide::new(&TestHost, Typed::none()), &temp, &target).unwrap_err();
    let text = error.message();
    assert!(text.contains("cannot move the copy into place"), "{text}");
    assert!(
        text.contains("cleaning up failed") && text.contains("partial"),
        "{text}"
    );
    assert!(temp.is_dir(), "it was left, and the error says so");
}
