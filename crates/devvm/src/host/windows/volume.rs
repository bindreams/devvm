//! What the volume of a file can do, and how much room it has.

use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetVolumeInformationByHandleW};
use windows_sys::Win32::System::SystemServices::FILE_SUPPORTS_POSIX_UNLINK_RENAME;

#[cfg(test)]
#[path = "volume_tests.rs"]
mod volume_tests;

/// [`Host::volume_has_posix_unlink_rename`](crate::host::Host::volume_has_posix_unlink_rename).
pub(super) fn has_posix_unlink_rename(file: &File) -> std::io::Result<bool> {
    let mut flags = 0u32;
    // SAFETY: the handle is open; the out pointer is a `u32`, and the other out pointers and buffers are null with
    // size 0.
    let read = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut flags,
            std::ptr::null_mut(),
            0,
        )
    };
    if read == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(flags & FILE_SUPPORTS_POSIX_UNLINK_RENAME != 0)
}

/// [`Host::free_space`](crate::host::Host::free_space) of `existing`, a directory that is there: the bytes free to the
/// caller, which a quota can make fewer than the volume has.
pub(super) fn free_bytes(existing: &Path) -> std::io::Result<u64> {
    let path = super::wide(existing)?;
    let mut free = 0u64;
    // SAFETY: `path` is NUL-terminated; the out pointer is a `u64`, and the other two out pointers are null, which
    // the call allows.
    let read = unsafe { GetDiskFreeSpaceExW(path.as_ptr(), &raw mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    if read == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(free)
}
