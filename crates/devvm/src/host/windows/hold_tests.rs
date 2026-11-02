use std::path::Path;

use crate::host::{HoldOutcome, current};
use crate::test_support::fixtures::unicode_home;

/// The access a hold is opened with includes `DELETE`, which share mode 0 still refuses to everyone else.
#[skuld::test]
fn a_held_file_can_be_neither_deleted_nor_renamed_by_anyone_else(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.sock.hold");
    let hold = current().hold_new(&path).unwrap();

    assert!(std::fs::remove_file(&path).is_err());
    assert!(std::fs::rename(&path, home.join("y")).is_err());

    assert!(path.exists());
    drop(hold);
}

#[skuld::test]
fn a_deleted_hold_leaves_no_deleting_name_once_its_duplicates_are_closed(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.sock.hold");
    let hold = current().hold_new(&path).unwrap();
    let duplicate = hold.file().try_clone().unwrap();

    hold.delete().unwrap();
    drop(duplicate);

    assert_eq!(std::fs::read_dir(home).unwrap().count(), 0);
}

/// A delete that died after renaming leaves `<name>.del`; the next delete of a file at that name replaces it.
#[skuld::test]
fn a_delete_replaces_a_deleting_name_a_dead_delete_left(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.sock.hold");
    std::fs::write(home.join("x.sock.hold.del"), b"left").unwrap();
    let hold = current().hold_new(&path).unwrap();

    hold.delete().unwrap();

    assert_eq!(std::fs::read_dir(home).unwrap().count(), 0);
    assert!(matches!(current().hold_existing(&path).unwrap(), HoldOutcome::Absent));
}
