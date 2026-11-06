//! What the volume of a file can do.

use std::fs::File;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;
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
