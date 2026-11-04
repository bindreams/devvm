//! A hold on Windows: the file open with share mode 0.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, NTSTATUS, STATUS_DELETE_PENDING};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_RENAME_INFO, FileDispositionInfo, FileRenameInfo, SetFileInformationByHandle,
};

use super::file_id;
use crate::host::{DELETING_SUFFIX, FileId, Hold, HoldOutcome};

#[derive(Debug)]
pub(super) struct WindowsHold {
    file: File,
    /// Where the file was taken, absolute: the directory it is renamed in before it is deleted.
    path: PathBuf,
}

impl WindowsHold {
    fn of(file: File, path: &Path) -> std::io::Result<WindowsHold> {
        Ok(WindowsHold {
            file,
            path: std::path::absolute(path)?,
        })
    }

    /// Renames the file through its handle to `<name>.del`, replacing a file a dead deleter left there. The name it had
    /// is free from here on, though the file keeps existing until every handle to it is closed.
    fn rename_away(&self) -> std::io::Result<()> {
        let mut name: OsString = self.path.as_os_str().to_owned();
        name.push(DELETING_SUFFIX);
        let name: Vec<u16> = name.encode_wide().collect();
        let name_bytes = name.len() * size_of::<u16>();
        // `u64` units give the buffer the alignment of `FILE_RENAME_INFO`.
        let mut buffer =
            vec![0u64; (offset_of!(FILE_RENAME_INFO, FileName) + name_bytes).div_ceil(size_of::<u64>()) + 1];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: the buffer is zeroed, aligned for `FILE_RENAME_INFO` and holds the header and the name, whose
        // characters are copied to where `FileName` begins.
        unsafe {
            (*info).Anonymous.ReplaceIfExists = true;
            (*info).FileNameLength = name_bytes as u32;
            std::ptr::copy_nonoverlapping(name.as_ptr(), (&raw mut (*info).FileName).cast::<u16>(), name.len());
        }
        // SAFETY: the handle is open with `DELETE` access, and the buffer is a `FILE_RENAME_INFO` of the size passed.
        let renamed = unsafe {
            SetFileInformationByHandle(
                self.file.as_raw_handle(),
                FileRenameInfo,
                info.cast(),
                (buffer.len() * size_of::<u64>()) as u32,
            )
        };
        if renamed == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

// `windows-sys` has no binding for it.
#[link(name = "ntdll")]
unsafe extern "system" {
    /// The `NTSTATUS` the calling thread's last system call ended with, which Win32 maps to an error code that can
    /// stand for several statuses.
    fn RtlGetLastNtStatus() -> NTSTATUS;
}

/// Whether `error`, from removing a deleting name, says that somebody else is deleting the file: it is held, or its
/// deletion is pending. A pending deletion is `ERROR_ACCESS_DENIED`, like a real refusal, so the status tells them
/// apart. Call it straight after the failed call: the status is the thread's last.
fn is_deleted_by_another(error: &std::io::Error) -> bool {
    match error.raw_os_error() {
        Some(code) if code == ERROR_SHARING_VIOLATION as i32 => true,
        // SAFETY: reads a thread-local status; no argument.
        Some(code) if code == ERROR_ACCESS_DENIED as i32 => unsafe { RtlGetLastNtStatus() == STATUS_DELETE_PENDING },
        _ => false,
    }
}

/// [`Host::remove_deleting_name`](crate::host::Host::remove_deleting_name).
pub(super) fn remove_deleting_name(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if is_deleted_by_another(&e) => Ok(()),
        other => other,
    }
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
    Ok(Box::new(WindowsHold::of(file, path)?))
}

/// Holds the file at `path` if nobody else does. A link at the name is opened itself, not followed, and refused.
pub(super) fn existing(path: &Path) -> std::io::Result<HoldOutcome> {
    match options().custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path) {
        Ok(file) if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is a link", path.display()),
        )),
        Ok(file) => Ok(HoldOutcome::Taken(Box::new(WindowsHold::of(file, path)?))),
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

    /// Renames the file away through the handle and marks it for deletion, so no name is looked up again. A deleted
    /// file keeps its name until the last handle to it closes (a name that is delete-pending refuses a new file with
    /// `ERROR_ACCESS_DENIED`), and a handle can outlive this one: a child's, or a scanner's. The rename frees the name
    /// at once; the file goes when the last handle closes.
    fn delete(self: Box<Self>) -> std::io::Result<()> {
        self.rename_away()?;
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

#[cfg(test)]
#[path = "hold_tests.rs"]
mod hold_tests;
