use std::path::Path;

use super::{quarantine_file, quarantine_path};
use crate::test_support::fixtures::{MemoryProgress, unicode_home};

#[skuld::test]
fn a_quarantined_file_keeps_its_name_in_a_fresh_directory_under_the_root_and_is_said(
    #[fixture(unicode_home)] home: &Path,
) {
    let file = home.join("junk.json");
    std::fs::write(&file, b"junk").unwrap();
    let root = home.join("quarantine");
    let progress = MemoryProgress::default();

    let target = quarantine_file(&root, "iso-memo", &file, &progress).unwrap().unwrap();
    assert!(target.starts_with(&root), "{}", target.display());
    assert_eq!(target.file_name(), file.file_name());
    assert!(
        target
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("iso-memo-"),
        "{}",
        target.display()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"junk");
    assert!(!file.exists());
    assert!(
        progress.messages().iter().any(|message| message.contains("unreadable")),
        "{:?}",
        progress.messages()
    );
}

#[skuld::test]
fn a_file_that_is_not_there_is_none_and_leaves_no_directory(#[fixture(unicode_home)] home: &Path) {
    let root = home.join("quarantine");
    let progress = MemoryProgress::default();
    assert_eq!(
        quarantine_file(&root, "x", &home.join("gone"), &progress).unwrap(),
        None
    );
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    assert_eq!(progress.messages(), Vec::<String>::new());
}

#[skuld::test]
fn two_paths_under_one_label_differ(#[fixture(unicode_home)] home: &Path) {
    let root = home.join("quarantine");
    assert_ne!(
        quarantine_path(&root, "x").unwrap(),
        quarantine_path(&root, "x").unwrap()
    );
}
