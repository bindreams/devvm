use std::path::Path;

use super::without_trailing_separator;

/// A root is a directory: stripping its separator would name the drive's current directory (`C:`) or the volume device
/// (`\?\C:`).
#[cfg(windows)]
#[skuld::test]
fn a_drive_root_keeps_its_separator() {
    for root in [r"C:\", r"D:\", "C:/", r"\?\C:\", r"C:\\", r"\server\share\"] {
        assert_eq!(without_trailing_separator(Path::new(root)), Path::new(root), "{root}");
        assert!(without_trailing_separator(Path::new(root)).has_root(), "{root}");
    }
    assert_eq!(
        without_trailing_separator(Path::new(r"C:\")).join("f.txt"),
        Path::new(r"C:\f.txt")
    );
}

#[skuld::test]
fn a_trailing_separator_is_dropped_from_anything_but_a_root() {
    assert_eq!(without_trailing_separator(Path::new("/")), Path::new("/"));
    assert_eq!(without_trailing_separator(Path::new("a/b/")), Path::new("a/b"));
    assert_eq!(without_trailing_separator(Path::new("a/b")), Path::new("a/b"));
    #[cfg(windows)]
    {
        assert_eq!(
            without_trailing_separator(Path::new(r"C:\work\")),
            Path::new(r"C:\work")
        );
        assert_eq!(without_trailing_separator(Path::new("C:work/")), Path::new("C:work"));
    }
}
