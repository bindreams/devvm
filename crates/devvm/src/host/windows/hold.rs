//! A hold on Windows: the file open with share mode 0.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::mem::{offset_of, size_of};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, INVALID_HANDLE_VALUE, NTSTATUS, STATUS_DELETE_PENDING,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_RENAME_INFO, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfoEx, FileRenameInfo, OPEN_EXISTING,
    SetFileInformationByHandle,
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

/// Whether a failed call on a deleting name says that somebody else is deleting the file: it is held, or its deletion
/// is pending. A pending deletion is `ERROR_ACCESS_DENIED`, like a real refusal, so the `status` tells them apart.
fn is_deleted_by_another(error: &std::io::Error, status: NTSTATUS) -> bool {
    match error.raw_os_error() {
        Some(code) if code == ERROR_SHARING_VIOLATION as i32 => true,
        Some(code) if code == ERROR_ACCESS_DENIED as i32 => status == STATUS_DELETE_PENDING,
        _ => false,
    }
}

/// The outcome of `remove_deleting_name` for a call that has just failed: the `NTSTATUS` is read at once, before
/// anything else replaces the thread's last.
fn after_failure() -> std::io::Result<()> {
    let error = std::io::Error::last_os_error();
    // SAFETY: reads a thread-local status; no argument.
    let status = unsafe { RtlGetLastNtStatus() };
    if is_deleted_by_another(&error, status) {
        Ok(())
    } else {
        Err(error)
    }
}

/// [`Host::remove_deleting_name`](crate::host::Host::remove_deleting_name). The name is opened once, for `DELETE`,
/// without following a link and without refusing anybody else's access, and the file is marked deleted through that
/// handle, so no attribute is changed and no name is looked up twice. The marking has POSIX semantics, which unlink
/// the name at once, and ignores the read-only attribute. A file that is held, or whose deletion is pending, is
/// somebody else's to finish. Any other failure is the Win32 error as it is.
pub(super) fn remove_deleting_name(path: &Path) -> std::io::Result<()> {
    let path = wide(path)?;
    // SAFETY: `path` is NUL-terminated, and a null security descriptor and template are allowed.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return after_failure();
    }
    // SAFETY: `handle` is a valid handle that nothing else owns.
    let file = unsafe { File::from_raw_handle(handle) };
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: the handle is open with `DELETE` access, and the buffer is a `FILE_DISPOSITION_INFO_EX` of the size passed.
    let marked = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            (&raw const info).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if marked == 0 {
        return after_failure();
    }
    Ok(())
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

    /// Renames the file away through the handle and marks it for deletion, so no name is looked up again. A handle to
    /// the same file object can outlive this one (a child's inherited copy), and the name stays until that file object
    /// closes; the rename frees the final name at once. The marking has POSIX semantics, so the `.del` name goes when
    /// the file object closes even while somebody else's open (a scanner's) lasts.
    fn delete(self: Box<Self>) -> std::io::Result<()> {
        self.rename_away()?;
        let info = FILE_DISPOSITION_INFO_EX {
            Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        };
        // SAFETY: the handle is open with `DELETE` access, and the buffer is a `FILE_DISPOSITION_INFO_EX` of the size
        // passed.
        let marked = unsafe {
            SetFileInformationByHandle(
                self.file.as_raw_handle(),
                FileDispositionInfoEx,
                (&raw const info).cast(),
                size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
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
