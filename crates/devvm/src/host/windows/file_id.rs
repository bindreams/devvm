//! A file's identity on NTFS and ReFS: `FileIdInfo`.

use std::fs::OpenOptions;
use std::mem::{size_of, zeroed};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx,
};

use crate::host::FileId;

/// The identity of the file `path` names. The open asks for no access to the contents, so a file held with share mode 0
/// can be read (the share check looks at data access only), and it does not follow a link or junction: the id is the
/// link's own. Without `FILE_FLAG_OPEN_REPARSE_POINT` an AF_UNIX socket file cannot be opened at all (error 1920).
pub(super) fn of(path: &Path) -> std::io::Result<FileId> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    // SAFETY: an all-zero `FILE_ID_INFO` is a valid value (integers and a byte array).
    let mut info: FILE_ID_INFO = unsafe { zeroed() };
    // SAFETY: the handle is open, and the buffer is a `FILE_ID_INFO` of the size passed.
    let read = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if read == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(FileId {
        volume: info.VolumeSerialNumber,
        file: u128::from_le_bytes(info.FileId.Identifier),
    })
}
