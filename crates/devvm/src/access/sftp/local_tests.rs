use std::path::Path;

use super::{lexically_normal, without_trailing_separator};

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
