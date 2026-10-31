use std::path::Path;

use crate::host::{Entry, Host};
use crate::test_support::fixtures::unicode_home;

use super::super::WindowsHost;

fn open(root: &Path, rel: &str) -> Entry {
    WindowsHost
        .open_entry(root, rel)
        .unwrap_or_else(|e| panic!("open_entry({rel}): {e}"))
}

#[skuld::test]
fn a_junction_above_the_entry_is_named_and_not_followed(#[fixture(unicode_home)] home: &Path) {
    let outside = home.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("f.txt"), b"not in the root").unwrap();
    let root = home.join("root");
    std::fs::create_dir_all(root.join("a")).unwrap();
    junction::create(&outside, root.join("a").join("b")).unwrap();
    for rel in ["a/b/f.txt", "a/b/gone", "a/b/deeper/f.txt"] {
        match open(&root, rel) {
            Entry::BelowSymlink(found) => assert_eq!(found, "a/b", "{rel}"),
            other => panic!("{rel}: {other:?}"),
        }
    }
}

#[skuld::test]
fn a_junction_at_the_end_is_a_symlink(#[fixture(unicode_home)] home: &Path) {
    let target = home.join("target");
    std::fs::create_dir(&target).unwrap();
    junction::create(&target, home.join("junction")).unwrap();
    assert!(matches!(open(home, "junction"), Entry::Symlink));
}

#[skuld::test]
fn a_name_is_found_whatever_its_case(#[fixture(unicode_home)] home: &Path) {
    std::fs::create_dir(home.join("Dir")).unwrap();
    std::fs::write(home.join("Dir").join("File.TXT"), b"x").unwrap();
    assert!(matches!(open(home, "dIR/file.txt"), Entry::File(_)));
}

#[skuld::test]
fn a_backslash_is_not_a_separator_in_a_name(#[fixture(unicode_home)] home: &Path) {
    std::fs::create_dir(home.join("a")).unwrap();
    std::fs::write(home.join("a").join("f.txt"), b"x").unwrap();
    let error = WindowsHost.open_entry(home, r"a\f.txt").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error}");
}

#[skuld::test]
fn a_name_windows_cannot_hold_is_absent(#[fixture(unicode_home)] home: &Path) {
    assert!(matches!(open(home, "a|b"), Entry::Absent));
}
