use std::fs::OpenOptions;
use std::mem::size_of;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_DISPOSITION_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo,
    SetFileInformationByHandle,
};

use crate::host::{HoldOutcome, current};
use crate::test_groups::{Group, symlinks};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::symlink::try_symlink;

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
/// A name that is a link is not opened through: what it leads to is somebody else's file, and holding it would lock
/// that file against its users.
#[skuld::test]
fn holding_an_existing_name_that_is_a_symlink_opens_nothing_through_it(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(symlinks)] _group: &Group,
) {
    let victim = home.join("victim");
    std::fs::write(&victim, b"theirs").unwrap();
    let link = home.join("x.sock.hold");
    try_symlink(&victim, &link).unwrap_or_else(|e| {
        panic!(
            "cannot create a symlink: {e} (it needs Developer Mode or an elevated token), or {}",
            symlinks::OPT_OUT
        )
    });

    assert!(current().hold_existing(&link).is_err());

    assert_eq!(std::fs::read(&victim).unwrap(), b"theirs");
    OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&victim)
        .expect("nobody has the victim open");
}

// A leftover deleting name --------------------------------------------------------------------------------------------

#[skuld::test]
fn removing_a_deleting_name_deletes_a_file_nobody_has_open(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.sock.hold.del");
    std::fs::write(&path, b"left").unwrap();

    current().remove_deleting_name(&path).unwrap();

    assert!(!path.exists());
}

/// A delete that is running holds the file (share mode 0) until it has renamed and marked it.
#[skuld::test]
fn removing_a_deleting_name_someone_holds_is_no_error_and_leaves_it(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.sock.hold.del");
    let hold = current().hold_new(&path).unwrap();

    current().remove_deleting_name(&path).unwrap();

    assert!(path.exists());
    drop(hold);
}

/// A file marked for deletion keeps its name while another handle is open, and an open of it is
/// `ERROR_ACCESS_DENIED` (`STATUS_DELETE_PENDING`).
#[skuld::test]
fn removing_a_deleting_name_whose_deletion_is_pending_is_no_error(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.sock.hold.del");
    std::fs::write(&path, b"left").unwrap();
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    let keeper = OpenOptions::new().read(true).share_mode(share).open(&path).unwrap();
    let marker = OpenOptions::new()
        .read(true)
        .access_mode(DELETE)
        .share_mode(share)
        .open(&path)
        .unwrap();
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the handle is open with `DELETE` access, and the buffer is a `FILE_DISPOSITION_INFO` of the size passed.
    let marked = unsafe {
        SetFileInformationByHandle(
            marker.as_raw_handle(),
            FileDispositionInfo,
            (&raw const info).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    assert_ne!(marked, 0, "{}", std::io::Error::last_os_error());
    drop(marker);
    let refused = std::fs::File::open(&path).unwrap_err();
    assert_eq!(refused.raw_os_error(), Some(5), "the name is delete-pending: {refused}");

    current().remove_deleting_name(&path).unwrap();

    drop(keeper);
    assert!(!path.exists());
}

/// `ERROR_ACCESS_DENIED` that is not a pending deletion still fails: a directory is refused the same way.
#[skuld::test]
fn removing_a_deleting_name_fails_on_an_access_denied_that_is_no_pending_deletion(
    #[fixture(unicode_home)] home: &Path,
) {
    let path = home.join("x.sock.hold.del");
    std::fs::create_dir(&path).unwrap();

    let error = current().remove_deleting_name(&path).unwrap_err();

    assert_eq!(error.raw_os_error(), Some(5), "{error}");
}

#[skuld::test]
fn removing_a_deleting_name_that_is_not_there_is_not_found(#[fixture(unicode_home)] home: &Path) {
    let error = current()
        .remove_deleting_name(&home.join("x.sock.hold.del"))
        .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}
