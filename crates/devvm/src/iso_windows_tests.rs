use std::fs::OpenOptions;
use std::path::Path;

use super::{IsoMemo, key_iso, read_install_iso};
use crate::host::Host;
use crate::iso::Known;
use crate::test_support::fixtures::{MemoryProgress, iso_fixture, unicode_home};
use crate::test_support::host::TestHost;
use crate::{Dirs, ErrorKind};

/// Everything that could change an ISO under a build, tried on `iso`: each fails while it is held.
fn assert_cannot_be_changed(home: &Path, iso: &Path) {
    let other = home.join("other.iso");
    std::fs::write(&other, b"somewhere else").unwrap();
    assert!(OpenOptions::new().write(true).open(iso).is_err(), "written in place");
    assert!(std::fs::remove_file(iso).is_err(), "deleted");
    assert!(
        std::fs::rename(&other, iso).is_err(),
        "replaced by a rename onto its name"
    );
    assert!(std::fs::rename(iso, home.join("moved.iso")).is_err(), "renamed away");
    std::fs::remove_file(&other).unwrap();
}

#[skuld::test]
fn an_iso_is_held_from_the_key_until_the_install_that_carries_it_is_dropped(#[fixture(unicode_home)] home: &Path) {
    let iso = home.join("win.iso");
    std::fs::copy(iso_fixture("single-image.iso"), &iso).unwrap();
    let memo = IsoMemo::new(&Dirs::under(home));
    let progress = MemoryProgress::default();

    let root = key_iso(&TestHost, &memo, &iso, None, &progress).unwrap();
    assert_cannot_be_changed(home, &iso);

    let install = read_install_iso(root).unwrap();
    assert_cannot_be_changed(home, &iso);
    assert_eq!(install.iso.path(), iso);

    drop(install);
    std::fs::rename(&iso, home.join("moved.iso")).unwrap();
}

#[skuld::test]
fn a_memo_hit_holds_the_iso_too(#[fixture(unicode_home)] home: &Path) {
    let iso = home.join("win.iso");
    std::fs::copy(iso_fixture("single-image.iso"), &iso).unwrap();
    let memo = IsoMemo::new(&Dirs::under(home));
    let progress = MemoryProgress::default();
    key_iso(&TestHost, &memo, &iso, None, &progress).unwrap();

    let hit = key_iso(&TestHost, &memo, &iso, None, &progress).unwrap();
    assert_cannot_be_changed(home, &iso);
    drop(hit);
}

#[skuld::test]
fn an_iso_that_another_program_is_writing_is_refused_naming_it_and_the_reason(#[fixture(unicode_home)] home: &Path) {
    let iso = home.join("win.iso");
    std::fs::copy(iso_fixture("single-image.iso"), &iso).unwrap();
    let _writer = OpenOptions::new().write(true).open(&iso).unwrap();
    let memo = IsoMemo::new(&Dirs::under(home));

    let error = key_iso(&TestHost, &memo, &iso, None, &MemoryProgress::default())
        .err()
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains("win.iso"), "{error}");
    assert!(error.message().contains("open for writing"), "{error}");
}

#[skuld::test]
fn spellings_that_differ_in_case_or_by_a_junction_are_one_iso_with_one_memo(#[fixture(unicode_home)] home: &Path) {
    let real = home.join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::copy(iso_fixture("single-image.iso"), real.join("win.iso")).unwrap();
    junction::create(&real, home.join("alias")).unwrap();
    let memo = IsoMemo::new(&Dirs::under(home));
    let progress = MemoryProgress::default();

    let roots: Vec<_> = [
        real.join("win.iso"),
        real.join("WIN.ISO"),
        home.join("alias").join("win.iso"),
        home.join("ALIAS").join("Win.Iso"),
    ]
    .iter()
    .map(|spelling| key_iso(&TestHost, &memo, spelling, None, &progress).unwrap().sha256)
    .collect();

    assert!(roots.windows(2).all(|pair| pair[0] == pair[1]), "{roots:?}");
    assert_eq!(memo.known().unwrap().len(), 1);
    let hashings = progress
        .messages()
        .iter()
        .filter(|message| message.starts_with("hashing ") && message.contains(": 0 of "))
        .count();
    assert_eq!(hashings, 1);
}

#[skuld::test]
fn the_final_path_of_a_held_iso_still_leads_to_it_after_a_junction_in_its_path_is_pointed_elsewhere(
    #[fixture(unicode_home)] home: &Path,
) {
    let (real, other) = (home.join("real"), home.join("other"));
    std::fs::create_dir(&real).unwrap();
    std::fs::create_dir(&other).unwrap();
    std::fs::copy(iso_fixture("single-image.iso"), real.join("win.iso")).unwrap();
    std::fs::copy(iso_fixture("three-images.iso"), other.join("win.iso")).unwrap();
    let link = home.join("link");
    junction::create(&real, &link).unwrap();
    let memo = IsoMemo::new(&Dirs::under(home));
    let root = key_iso(
        &TestHost,
        &memo,
        &link.join("win.iso"),
        None,
        &MemoryProgress::default(),
    )
    .unwrap();
    let install = read_install_iso(root).unwrap();

    junction::delete(&link).unwrap();
    std::fs::remove_dir(&link).unwrap();
    junction::create(&other, &link).unwrap();

    let held = TestHost.file_id(&real.join("win.iso")).unwrap();
    let final_path = install.iso.final_path(&TestHost).unwrap();
    assert_eq!(TestHost.file_id(&final_path).unwrap(), held);
    assert_ne!(
        TestHost.file_id(&link.join("win.iso")).unwrap(),
        held,
        "the name the ISO was opened by leads to another file now"
    );
    assert!(!final_path.starts_with(&link), "{}", final_path.display());
}

#[skuld::test]
fn an_iso_replaced_and_then_reached_by_another_spelling_of_its_path_leaves_one_memo(
    #[fixture(unicode_home)] home: &Path,
) {
    let real = home.join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::copy(iso_fixture("single-image.iso"), real.join("win.iso")).unwrap();
    junction::create(&real, home.join("alias")).unwrap();
    let memo = IsoMemo::new(&Dirs::under(home));
    let progress = MemoryProgress::default();
    let old = key_iso(&TestHost, &memo, &real.join("win.iso"), None, &progress).unwrap();
    drop(old);

    std::fs::copy(iso_fixture("install-esd.iso"), real.join("new.iso")).unwrap();
    std::fs::rename(real.join("new.iso"), real.join("win.iso")).unwrap();
    let new = key_iso(&TestHost, &memo, &home.join("alias").join("win.iso"), None, &progress).unwrap();

    let known = memo.known().unwrap();
    assert!(
        matches!(&known[..], [Known::Iso { sha256, .. }] if *sha256 == new.sha256),
        "the replaced ISO's memo is gone: {known:?}"
    );
}
