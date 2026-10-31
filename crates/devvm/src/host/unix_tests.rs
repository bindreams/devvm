use std::path::Path;

use rustix::io::Errno;

use super::{lacks_a_rename_that_refuses_to_replace, publish_with};
use crate::test_support::fixtures::unicode_home;

#[skuld::test]
fn only_a_missing_rename_is_told_from_a_refusal_of_this_one() {
    for lacking in [Errno::INVAL, Errno::NOSYS, Errno::NOTSUP, Errno::OPNOTSUPP] {
        assert!(lacks_a_rename_that_refuses_to_replace(lacking), "{lacking}");
    }
    for refusal in [
        Errno::EXIST,
        Errno::NOTEMPTY,
        Errno::NOENT,
        Errno::ACCESS,
        Errno::PERM,
        Errno::XDEV,
        Errno::ROFS,
        Errno::NOSPC,
        Errno::IO,
    ] {
        assert!(!lacks_a_rename_that_refuses_to_replace(refusal), "{refusal}");
    }
}

fn dir_with_a_file(home: &Path, name: &str) -> std::path::PathBuf {
    let dir = home.join(name);
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("held"), name).unwrap();
    dir
}

#[skuld::test]
fn a_rename_the_file_system_lacks_falls_back_to_claiming_the_name(#[fixture(unicode_home)] home: &Path) {
    for lacking in [Errno::INVAL, Errno::NOSYS, Errno::NOTSUP, Errno::OPNOTSUPP] {
        let (from, to) = (dir_with_a_file(home, "from"), home.join("to"));
        publish_with(&from, &to, |_, _| Err(lacking)).unwrap();
        assert!(!from.exists(), "{lacking}");
        assert_eq!(std::fs::read_to_string(to.join("held")).unwrap(), "from", "{lacking}");
        std::fs::remove_dir_all(&to).unwrap();
    }
}

#[skuld::test]
fn the_fallback_still_refuses_a_name_that_is_taken(#[fixture(unicode_home)] home: &Path) {
    let (from, to) = (dir_with_a_file(home, "from"), home.join("taken"));
    std::fs::create_dir(&to).unwrap();
    let error = publish_with(&from, &to, |_, _| Err(Errno::NOSYS)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
    assert!(from.join("held").exists());
}

#[skuld::test]
fn a_refusal_of_this_rename_is_the_error_and_nothing_is_claimed(#[fixture(unicode_home)] home: &Path) {
    let (from, to) = (dir_with_a_file(home, "from"), home.join("to"));
    let error = publish_with(&from, &to, |_, _| Err(Errno::ACCESS)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied, "{error}");
    assert!(!to.exists(), "a name was claimed");
    assert!(from.join("held").exists());
}
