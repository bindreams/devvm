//! A hold on Windows: the file open with share mode 0.

use std::fs::{File, OpenOptions};
use std::mem::size_of;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_DISPOSITION_INFO, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FileDispositionInfo,
    SetFileInformationByHandle,
};

use super::file_id;
use crate::host::{FileId, Hold, HoldOutcome};

#[derive(Debug)]
pub(super) struct WindowsHold {
    file: File,
}

/// The options of a hold. Share mode 0 refuses every other open of the file, including DELETE, for as long as any
/// handle to this open file exists, the inherited ones too. `DELETE` access is for [`Hold::delete`].
fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .access_mode(DELETE | FILE_GENERIC_READ | FILE_GENERIC_WRITE)
        .share_mode(0);
    options
}

pub(super) fn new(path: &Path) -> std::io::Result<Box<dyn Hold>> {
    let file = options().create_new(true).open(path)?;
    Ok(Box::new(WindowsHold { file }))
}

pub(super) fn existing(path: &Path) -> std::io::Result<HoldOutcome> {
    match options().open(path) {
        Ok(file) => Ok(HoldOutcome::Taken(Box::new(WindowsHold { file }))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HoldOutcome::Absent),
        Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => Ok(HoldOutcome::Busy),
        Err(e) => Err(e),
    }
}

impl Hold for WindowsHold {
    fn file(&self) -> &File {
        &self.file
    }

    fn id(&self) -> std::io::Result<FileId> {
        file_id::of_handle(&self.file)
    }

    /// Marks the file for deletion through the handle, so no name is looked up again. It goes when the last handle to
    /// it closes, which is this one if nobody inherited another.
    fn delete(self: Box<Self>) -> std::io::Result<()> {
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: the handle is open with `DELETE` access, and the buffer is a `FILE_DISPOSITION_INFO` of the size passed.
        let marked = unsafe {
            SetFileInformationByHandle(
                self.file.as_raw_handle(),
                FileDispositionInfo,
                (&raw const info).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
        if marked == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}
