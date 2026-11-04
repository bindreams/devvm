use std::fs::OpenOptions;
use std::mem::size_of;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_DISPOSITION_INFO,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo, SetFileAttributesW,
    SetFileInformationByHandle,
};

use super::super::wide;
use super::super::windows_tests::{long_dir, with_nul};
use crate::host::{HoldOutcome, current};
use crate::test_groups::{Group, symlinks};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::symlink::symlink;

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
    symlink(&victim, &link);

    let refused = current().hold_existing(&link).unwrap_err();

    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidInput, "{refused}");
    assert!(refused.to_string().ends_with("is a link"), "{refused}");
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
    assert_eq!(
        refused.raw_os_error(),
        Some(ERROR_ACCESS_DENIED as i32),
        "the name is delete-pending: {refused}"
    );

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

    assert_eq!(error.raw_os_error(), Some(ERROR_ACCESS_DENIED as i32), "{error}");
}

#[skuld::test]
fn removing_a_deleting_name_that_is_not_there_is_not_found(#[fixture(unicode_home)] home: &Path) {
    let error = current()
        .remove_deleting_name(&home.join("x.sock.hold.del"))
        .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

/// A path with a NUL is not read as far as the NUL: that would delete the file named before it.
#[skuld::test]
fn removing_a_deleting_name_with_a_nul_in_it_is_invalid_input_and_deletes_nothing(
    #[fixture(unicode_home)] home: &Path,
) {
    let path = home.join("x.sock.hold.del");
    std::fs::write(&path, b"left").unwrap();

    let error = current().remove_deleting_name(&with_nul(&path)).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error}");
    assert!(path.exists());
}

fn set_attributes(path: &Path, attributes: u32) -> std::io::Result<()> {
    let path = wide(path)?;
    // SAFETY: `path` is NUL-terminated.
    match unsafe { SetFileAttributesW(path.as_ptr(), attributes) } {
        0 => Err(std::io::Error::last_os_error()),
        _ => Ok(()),
    }
}

/// Removes the read-only file at `path` as a deleting name. The attribute is cleared, its error ignored, before the
/// result is asserted, so a failure leaves a file the test directory's cleanup can delete.
fn remove_read_only_deleting_name(path: &Path) {
    std::fs::write(path, b"left").unwrap();
    set_attributes(path, FILE_ATTRIBUTE_READONLY).unwrap();

    let removed = current().remove_deleting_name(path);

    let _ = set_attributes(path, FILE_ATTRIBUTE_NORMAL);
    removed.unwrap();
    assert!(!path.exists());
}

#[skuld::test]
fn a_read_only_deleting_name_is_removed(#[fixture(unicode_home)] home: &Path) {
    remove_read_only_deleting_name(&home.join("x.sock.hold.del"));
}

#[skuld::test]
fn a_read_only_deleting_name_deeper_than_max_path_is_removed(#[fixture(unicode_home)] home: &Path) {
    remove_read_only_deleting_name(&long_dir(home).join("x.sock.hold.del"));
}

/// No attribute of the file is changed to remove its name: one that a handle keeps open still has them all.
#[skuld::test]
fn removing_a_deleting_name_leaves_the_attributes_of_a_file_a_handle_keeps_open(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.sock.hold.del");
    std::fs::write(&path, b"left").unwrap();
    let attributes = FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_HIDDEN;
    set_attributes(&path, attributes).unwrap();
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    let keeper = OpenOptions::new().read(true).share_mode(share).open(&path).unwrap();

    let removed = current().remove_deleting_name(&path);

    let kept = keeper.metadata().unwrap().file_attributes();
    drop(keeper);
    let _ = set_attributes(&path, FILE_ATTRIBUTE_NORMAL);
    removed.unwrap();
    assert_eq!(kept & attributes, attributes);
}
