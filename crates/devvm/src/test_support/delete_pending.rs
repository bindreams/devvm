//! A name whose deletion is pending, which only Windows has.

use std::fs::{File, OpenOptions};
use std::mem::size_of;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_DISPOSITION_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo,
    SetFileInformationByHandle,
};

/// Marks the file at `path` for deletion and returns a handle that keeps it open: the name stays, delete-pending, until
/// the handle is dropped. Any open of the name fails with `ERROR_ACCESS_DENIED` (`STATUS_DELETE_PENDING`).
pub fn make_delete_pending(path: &Path) -> File {
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    let keeper = OpenOptions::new().read(true).share_mode(share).open(path).unwrap();
    let marker = OpenOptions::new()
        .read(true)
        .access_mode(DELETE)
        .share_mode(share)
        .open(path)
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
    keeper
}
