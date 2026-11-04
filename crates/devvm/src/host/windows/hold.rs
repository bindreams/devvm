//! A hold on Windows: the file open with share mode 0.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::mem::{offset_of, size_of};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, NTSTATUS, STATUS_CANNOT_DELETE, STATUS_DELETE_PENDING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, DeleteFileW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_RENAME_INFO, FileDispositionInfo,
    FileRenameInfo, SetFileAttributesW, SetFileInformationByHandle,
};

use super::file_id;
use super::wide;
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
        let mut name = wide(Path::new(&name))?;
        name.pop(); // the NUL: the name's length is stated, not terminated
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

/// Whether a failed `DeleteFileW` on a deleting name says that somebody else is deleting the file: it is held, or its
/// deletion is pending. A pending deletion is `ERROR_ACCESS_DENIED`, like a real refusal, so the `status` tells them
/// apart.
fn is_deleted_by_another(error: &std::io::Error, status: NTSTATUS) -> bool {
    match error.raw_os_error() {
        Some(code) if code == ERROR_SHARING_VIOLATION as i32 => true,
        Some(code) if code == ERROR_ACCESS_DENIED as i32 => status == STATUS_DELETE_PENDING,
        _ => false,
    }
}

/// One `DeleteFileW`. A failure carries the `NTSTATUS` read at once, before anything else replaces the thread's last.
fn delete_file(path: &[u16]) -> Result<(), (std::io::Error, NTSTATUS)> {
    // SAFETY: `path` is NUL-terminated.
    if unsafe { DeleteFileW(path.as_ptr()) } != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    // SAFETY: reads a thread-local status; no argument.
    Err((error, unsafe { RtlGetLastNtStatus() }))
}

/// [`Host::remove_deleting_name`](crate::host::Host::remove_deleting_name). The name is looked up by `DeleteFileW`
/// alone. A file that is held, or whose deletion is pending, is somebody else's to finish. A read-only file
/// (`STATUS_CANNOT_DELETE`) has the attribute cleared, and `DeleteFileW` runs once more with the same outcomes. Any
/// other failure is the Win32 error as it is.
pub(super) fn remove_deleting_name(path: &Path) -> std::io::Result<()> {
    let path = wide(path)?;
    let (error, status) = match delete_file(&path) {
        Ok(()) => return Ok(()),
        Err(failure) => failure,
    };
    if is_deleted_by_another(&error, status) {
        return Ok(());
    }
    if status != STATUS_CANNOT_DELETE {
        return Err(error);
    }
    // SAFETY: `path` is NUL-terminated.
    if unsafe { SetFileAttributesW(path.as_ptr(), FILE_ATTRIBUTE_NORMAL) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    match delete_file(&path) {
        Ok(()) => Ok(()),
        Err((error, status)) if is_deleted_by_another(&error, status) => Ok(()),
        Err((error, _)) => Err(error),
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
