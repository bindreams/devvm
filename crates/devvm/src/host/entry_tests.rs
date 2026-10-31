use std::io::Read;
use std::path::Path;

use super::{Entry, components};
use crate::host::current;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::symlink::symlink;

fn open(root: &Path, rel: &str) -> Entry {
    current()
        .open_entry(root, rel)
        .unwrap_or_else(|e| panic!("open_entry({rel}): {e}"))
}

#[skuld::test]
fn only_names_come_out_of_a_path_below_a_root() {
    assert_eq!(components("a/\u{416}/c.txt").unwrap(), ["a", "\u{416}", "c.txt"]);
    for bad in ["", "/a", "a/", "a//b", "./a", "a/../b", "..", "a/b\0"] {
        let error = components(bad).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{bad:?}");
    }
}

#[skuld::test]
fn a_regular_file_comes_back_open_with_its_bytes(#[fixture(unicode_home)] home: &Path) {
    std::fs::create_dir(home.join("dir")).unwrap();
    std::fs::write(home.join("dir").join("f.txt"), b"the bytes").unwrap();
    let Entry::File(mut file) = open(home, "dir/f.txt") else {
        panic!("a regular file is a File");
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"the bytes");
    assert_eq!(file.metadata().unwrap().len(), 9);
}

#[skuld::test]
fn a_directory_is_a_directory(#[fixture(unicode_home)] home: &Path) {
    std::fs::create_dir_all(home.join("a").join("b")).unwrap();
    assert!(matches!(open(home, "a/b"), Entry::Directory));
}

#[skuld::test]
fn what_is_not_there_is_absent_whichever_component_is_missing(#[fixture(unicode_home)] home: &Path) {
    std::fs::create_dir(home.join("dir")).unwrap();
    std::fs::write(home.join("file"), b"x").unwrap();
    for rel in [
        "gone",
        "dir/gone",
        "gone/deeper/still",
        "file/below",
        "file/below/deeper",
    ] {
        assert!(matches!(open(home, rel), Entry::Absent), "{rel}");
    }
}

#[skuld::test]
fn a_symlink_at_the_end_is_a_symlink_and_is_not_followed(#[fixture(unicode_home)] home: &Path) {
    std::fs::write(home.join("target.txt"), b"x").unwrap();
    std::fs::create_dir(home.join("target_dir")).unwrap();
    symlink(&home.join("target.txt"), &home.join("to_file"));
    symlink(&home.join("target_dir"), &home.join("to_dir"));
    symlink(&home.join("nowhere"), &home.join("dangling"));
    for rel in ["to_file", "to_dir", "dangling"] {
        assert!(matches!(open(home, rel), Entry::Symlink), "{rel}");
    }
}

#[skuld::test]
fn a_symlink_above_the_entry_is_named_and_not_followed(#[fixture(unicode_home)] home: &Path) {
    let outside = home.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("f.txt"), b"not in the root").unwrap();
    let root = home.join("root");
    std::fs::create_dir_all(root.join("a")).unwrap();
    symlink(&outside, &root.join("a").join("b"));
    symlink(&home.join("nowhere"), &root.join("dangling"));
    for (rel, link) in [
        ("a/b/f.txt", "a/b"),
        ("a/b/gone", "a/b"),
        ("a/b/deeper/f.txt", "a/b"),
        ("dangling/f.txt", "dangling"),
    ] {
        match open(&root, rel) {
            Entry::BelowSymlink(found) => assert_eq!(found, link, "{rel}"),
            other => panic!("{rel}: {other:?}"),
        }
    }
}

#[skuld::test]
fn a_root_that_is_itself_a_symlink_is_followed(#[fixture(unicode_home)] home: &Path) {
    let real = home.join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("f.txt"), b"x").unwrap();
    symlink(&real, &home.join("link"));
    assert!(matches!(open(&home.join("link"), "f.txt"), Entry::File(_)));
}

#[skuld::test]
fn a_root_that_is_not_there_is_an_error(#[fixture(unicode_home)] home: &Path) {
    let error = current().open_entry(&home.join("gone"), "f.txt").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
}

#[skuld::test]
fn a_path_that_is_not_below_the_root_is_refused(#[fixture(unicode_home)] home: &Path) {
    for rel in ["", "..", "a/../b", "/a"] {
        let error = current().open_entry(home, rel).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{rel:?}");
    }
}
