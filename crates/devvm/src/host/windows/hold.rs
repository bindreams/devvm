//! A hold on Windows: the file open with share mode 0.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::mem::{offset_of, size_of};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED,
    ERROR_SHARING_VIOLATION, INVALID_HANDLE_VALUE, NTSTATUS, STATUS_DELETE_PENDING,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_RENAME_INFO, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo, FileDispositionInfoEx, FileRenameInfo, FileRenameInfoEx,
    OPEN_EXISTING, SetFileInformationByHandle,
};

use super::file_id;
use super::wide;
use crate::host::{DELETING_SUFFIX, FileId, Hold, HoldOutcome};

/// `FILE_RENAME_FLAG_REPLACE_IF_EXISTS` and `FILE_RENAME_FLAG_POSIX_SEMANTICS`, the `Flags` of `FILE_RENAME_INFO`
/// (`ntifs.h`; `windows-sys` does not bind them). `std::fs::rename` passes the same two to `FileRenameInfoEx`.
const RENAME_REPLACE_IF_EXISTS: u32 = 0x1;
const RENAME_POSIX_SEMANTICS: u32 = 0x2;

/// Whether `error` says the file system does not do the `*Ex` form of a call. `std`'s `File::delete`
/// (`library/std/src/sys/fs/windows.rs`) falls back from `posix_delete` to `win32_delete` on exactly these three:
/// `ERROR_INVALID_PARAMETER` and `ERROR_NOT_SUPPORTED`, which `posix_delete` documents (FAT32, exFAT, SMB, Windows
/// before 10 1607), and `ERROR_INVALID_FUNCTION`.
fn is_unsupported(error: &std::io::Error) -> bool {
    [ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, ERROR_INVALID_FUNCTION]
        .iter()
        .any(|&unsupported| error.raw_os_error() == Some(unsupported as i32))
}

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

    /// Renames the file through its handle to `<name>.del`, replacing a file a dead deleter left there, even one that
    /// somebody has open (`FileRenameInfoEx`, POSIX semantics); where the file system has none, only one that nobody
    /// has open (`FileRenameInfo`). The name it had is free from here on, though the file keeps existing until every
    /// handle to it is closed.
    fn rename_away(&self) -> std::io::Result<()> {
        let mut name: OsString = self.path.as_os_str().to_owned();
        name.push(DELETING_SUFFIX);
        let mut name = wide(Path::new(&name))?;
        name.pop(); // the NUL: the name's length is stated, not terminated
        let name_bytes = name.len() * size_of::<u16>();
        // `u64` units give the buffer the alignment of `FILE_RENAME_INFO`.
        let mut buffer =
            vec![0u64; (offset_of!(FILE_RENAME_INFO, FileName) + name_bytes).div_ceil(size_of::<u64>()) + 1];
        let size = (buffer.len() * size_of::<u64>()) as u32;
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: the buffer is zeroed, aligned for `FILE_RENAME_INFO` and holds the header and the name, whose
        // characters are copied to where `FileName` begins.
        unsafe {
            (*info).FileNameLength = name_bytes as u32;
            std::ptr::copy_nonoverlapping(name.as_ptr(), (&raw mut (*info).FileName).cast::<u16>(), name.len());
            (*info).Anonymous.Flags = RENAME_REPLACE_IF_EXISTS | RENAME_POSIX_SEMANTICS;
        }
        // SAFETY: the handle is open with `DELETE` access, and the buffer is a `FILE_RENAME_INFO` of the size passed.
        let renamed =
            unsafe { SetFileInformationByHandle(self.file.as_raw_handle(), FileRenameInfoEx, info.cast(), size) };
        if renamed != 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if !is_unsupported(&error) {
            return Err(error);
        }
        // SAFETY: as above; the union's `Flags` are cleared, then its `ReplaceIfExists` is set.
        let renamed = unsafe {
            (*info).Anonymous.Flags = 0;
            (*info).Anonymous.ReplaceIfExists = true;
            SetFileInformationByHandle(self.file.as_raw_handle(), FileRenameInfo, info.cast(), size)
        };
        if renamed == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Marks the file deleted through its handle, with POSIX semantics, which unlink the name when the file object
    /// closes whoever else has the file open; where the file system has none, with `FileDispositionInfo`, which
    /// deletes it when the last handle closes (`std`'s `win32_delete`).
    fn mark_deleted(&self) -> std::io::Result<()> {
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
        if marked != 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if !is_unsupported(&error) {
            return Err(error);
        }
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: as above, with a `FILE_DISPOSITION_INFO`.
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

// `windows-sys` has no binding for it.
#[link(name = "ntdll")]
unsafe extern "system" {
    /// The `NTSTATUS` the calling thread's last system call ended with, which Win32 maps to an error code that can
    /// stand for several statuses.
    fn RtlGetLastNtStatus() -> NTSTATUS;
}

/// Whether the name at `path`, which `std::fs::remove_file` refused with `ERROR_ACCESS_DENIED`, is being deleted by
/// somebody else: its deletion is pending, or it is gone meanwhile. One open with no access rights tells. A pending
/// deletion refuses it as `ERROR_ACCESS_DENIED` too, so the `NTSTATUS`, read at once, tells it from a real refusal. An
/// open that succeeds, or fails otherwise, means the refusal was real.
fn is_deleted_by_another(path: &Path) -> bool {
    let Ok(path) = wide(path) else { return false };
    // SAFETY: `path` is NUL-terminated, and a null security descriptor and template are allowed.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle != INVALID_HANDLE_VALUE {
        // SAFETY: `handle` is a valid handle that nothing else owns; dropping it closes it.
        drop(unsafe { OwnedHandle::from_raw_handle(handle) });
        return false;
    }
    let code = std::io::Error::last_os_error().raw_os_error();
    // SAFETY: reads a thread-local status; no argument.
    let status = unsafe { RtlGetLastNtStatus() };
    status == STATUS_DELETE_PENDING || code == Some(ERROR_FILE_NOT_FOUND as i32)
}

/// [`Host::remove_deleting_name`](crate::host::Host::remove_deleting_name): `std::fs::remove_file`, which takes long
/// paths, refuses a NUL and removes a read-only file as `std` does. A file that is held (`ERROR_SHARING_VIOLATION`), or
/// whose deletion is pending, is somebody else's to finish. Any other failure is `std`'s error as it is.
pub(super) fn remove_deleting_name(path: &Path) -> std::io::Result<()> {
    let error = match std::fs::remove_file(path) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    match error.raw_os_error() {
        Some(code) if code == ERROR_SHARING_VIOLATION as i32 => Ok(()),
        Some(code) if code == ERROR_ACCESS_DENIED as i32 && is_deleted_by_another(path) => Ok(()),
        _ => Err(error),
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

    /// Renames the file away through the handle and marks it for deletion, so no name is looked up again. A handle to
    /// the same file object can outlive this one (a child's inherited copy), and the name stays until that file object
    /// closes; the rename frees the final name at once. Where the file system has POSIX semantics, the `.del` name goes
    /// when the file object closes even while somebody else's open (a scanner's) lasts; elsewhere it goes when the
    /// last handle closes.
    fn delete(self: Box<Self>) -> std::io::Result<()> {
        self.rename_away()?;
        self.mark_deleted()
    }
}

#[cfg(test)]
#[path = "hold_tests.rs"]
mod hold_tests;
