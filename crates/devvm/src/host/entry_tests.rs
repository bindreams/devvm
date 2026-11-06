use std::io::Read;
use std::path::Path;

use super::{Entry, is_absent, symlink_metadata_if_present};
use crate::host::current;
use crate::test_groups::{Group, symlinks};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::symlink::symlink;

fn open(root: &Path, rel: &str) -> Entry {
    current()
        .open_entry(root, rel)
        .unwrap_or_else(|e| panic!("open_entry({rel}): {e}"))
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
fn a_symlink_at_the_end_is_a_symlink_and_is_not_followed(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(symlinks)] _group: &Group,
) {
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
fn a_root_that_is_itself_a_symlink_is_followed(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(symlinks)] _group: &Group,
) {
    let real = home.join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("f.txt"), b"x").unwrap();
    symlink(&real, &home.join("link"));
    assert!(matches!(open(&home.join("link"), "f.txt"), Entry::File(_)));
}

#[skuld::test]
fn a_name_that_is_not_there_is_none_and_one_that_is_there_is_some(#[fixture(unicode_home)] home: &Path) {
    std::fs::write(home.join("file"), b"x").unwrap();

    assert!(symlink_metadata_if_present(&home.join("file")).unwrap().is_some());
    assert!(symlink_metadata_if_present(&home.join("gone")).unwrap().is_none());
    assert!(
        symlink_metadata_if_present(&home.join("file").join("below"))
            .unwrap()
            .is_none()
    );
}

/// A lookup that fails for another reason than absence says so: it is not an absent name.
#[skuld::test]
fn a_lookup_that_fails_for_another_reason_is_the_error(#[fixture(unicode_home)] home: &Path) {
    let error = symlink_metadata_if_present(&home.join("a\0b")).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[skuld::test]
fn a_link_is_looked_at_not_followed(#[fixture(unicode_home)] home: &Path, #[fixture(symlinks)] _group: &Group) {
    symlink(&home.join("nowhere"), &home.join("dangling"));

    let meta = symlink_metadata_if_present(&home.join("dangling")).unwrap().unwrap();

    assert!(meta.file_type().is_symlink());
}

/// An error with no OS code is told by its kind: `InvalidFilename` is a name that cannot exist only where the host's own
/// code says so, and every other failure is a lookup that did not find the answer.
#[skuld::test]
fn only_an_error_that_says_the_name_is_not_there_is_absent() {
    use std::io::ErrorKind::*;
    for kind in [NotFound, NotADirectory] {
        assert!(is_absent(&kind.into()), "{kind:?}");
    }
    for kind in [
        InvalidFilename,
        InvalidInput,
        PermissionDenied,
        TimedOut,
        HostUnreachable,
        NetworkUnreachable,
        DirectoryNotEmpty,
        AlreadyExists,
    ] {
        assert!(!is_absent(&kind.into()), "{kind:?}");
    }
}
