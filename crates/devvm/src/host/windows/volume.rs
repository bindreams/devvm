//! What the volume of a file can do.

use std::fs::File;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;
use windows_sys::Win32::System::SystemServices::FILE_SUPPORTS_POSIX_UNLINK_RENAME;

/// Whether the volume of `file` has POSIX unlink and rename, which instance lock files rely on: NTFS and ReFS have it;
/// exFAT, FAT32 and SMB do not.
pub(super) fn check_lock_volume(file: &File) -> std::io::Result<()> {
    let mut flags = 0u32;
    // SAFETY: the handle is open; the out pointer is a `u32`, and the other out pointers and buffers are null with
    // size 0, which the call allows.
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
    if flags & FILE_SUPPORTS_POSIX_UNLINK_RENAME == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the devvm state directory must be on NTFS or ReFS",
        ));
    }
    Ok(())
}
