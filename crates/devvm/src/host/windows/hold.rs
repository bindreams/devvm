//! A hold on Windows: the file open with share mode 0.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::mem::{offset_of, size_of};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, ERROR_SHARING_VIOLATION,
    INVALID_HANDLE_VALUE, NTSTATUS, STATUS_DELETE_PENDING,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO,
    FILE_DISPOSITION_INFO_EX, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_RENAME_INFO,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo, FileDispositionInfoEx, FileRenameInfo,
    FileRenameInfoEx, OPEN_EXISTING, SetFileInformationByHandle,
};
use windows_sys::Win32::System::WindowsProgramming::FILE_RENAME_FLAG_POSIX_SEMANTICS;

use super::file_id;
use super::wide;
use crate::host::{DELETING_SUFFIX, FileId, Hold, HoldOutcome};

/// Whether `error` says the file system does not do the `*Ex` form of a disposition. `std`'s `File::delete`
/// (`library/std/src/sys/fs/windows.rs`) falls back from `posix_delete` to `win32_delete` on exactly these three:
/// `ERROR_INVALID_PARAMETER` and `ERROR_NOT_SUPPORTED`, which `posix_delete` documents (FAT32, exFAT, SMB, Windows
/// before 10 1607), and `ERROR_INVALID_FUNCTION`.
fn is_unsupported(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error().map(|code| code as u32),
        Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION)
    )
}

// `windows-sys` has no binding for it.
#[link(name = "ntdll")]
unsafe extern "system" {
    /// The `NTSTATUS` the calling thread's last system call ended with, which Win32 maps to an error code that can
    /// stand for several statuses.
    fn RtlGetLastNtStatus() -> NTSTATUS;
}

/// A failed system call: its Win32 error and the `NTSTATUS` it ended with.
struct Failure {
    error: std::io::Error,
    status: NTSTATUS,
}

impl Failure {
    /// The failure of the calling thread's last system call, which must be the one that failed: both are read from the
    /// thread's last call, so nothing may call the system in between.
    fn last() -> Failure {
        let error = std::io::Error::last_os_error();
        // SAFETY: reads a thread-local status; no argument.
        let status = unsafe { RtlGetLastNtStatus() };
        Failure { error, status }
    }

    /// Whether the file is somebody else's to finish: held (`ERROR_SHARING_VIOLATION`), or its deletion is pending. A
    /// pending deletion is `ERROR_ACCESS_DENIED` like a real refusal, and the status tells them apart.
    fn is_another_deleters(&self) -> bool {
        match self.error.raw_os_error().map(|code| code as u32) {
            Some(ERROR_SHARING_VIOLATION) => true,
            Some(ERROR_ACCESS_DENIED) => self.status == STATUS_DELETE_PENDING,
            _ => false,
        }
    }
}

/// Marks `file`, open with `DELETE` access, for deletion: `FileDispositionInfoEx` with `FILE_DISPOSITION_FLAG_DELETE`
/// and `flags`. POSIX semantics unlink the name when the file object closes, whoever else has the file open. Where the
/// file system or Windows has no `*Ex` form, `FileDispositionInfo` deletes the file when the last handle closes
/// (`std`'s `win32_delete`), and `flags` do not apply.
fn mark_deleted(file: &File, flags: u32) -> Result<(), Failure> {
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | flags,
    };
    // SAFETY: the handle is open with `DELETE` access, and the buffer is a `FILE_DISPOSITION_INFO_EX` of the size
    // passed.
    let marked = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            (&raw const info).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if marked != 0 {
        return Ok(());
    }
    let failure = Failure::last();
    if !is_unsupported(&failure.error) {
        return Err(failure);
    }
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: as above, with a `FILE_DISPOSITION_INFO`.
    let marked = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&raw const info).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if marked == 0 {
        return Err(Failure::last());
    }
    Ok(())
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

    /// Renames the file through its handle to `<name>.<16 random hex>.del`, with POSIX semantics, which let the rename
    /// go through while somebody has the file open (`FileRenameInfoEx`); where the file system has none,
    /// `FileRenameInfo`. The name is drawn for this delete alone, so it is never one that another delete, a duplicate
    /// of another file or a scanner still has. The name the file had is free from here on, though the file keeps
    /// existing until every handle to it is closed.
    ///
    /// Falls back on `ERROR_INVALID_PARAMETER` only, which a missing `*Ex` class gives, and returns the `*Ex` error
    /// when the fallback fails too: `std`'s `unlink` and `rename` (`library/std/src/sys/fs/windows.rs`) return "the
    /// original error if any of the above fails".
    fn rename_away(&self) -> std::io::Result<()> {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).expect("the OS random number generator is available");
        let mut name: OsString = self.path.as_os_str().to_owned();
        name.push(format!(".{}{DELETING_SUFFIX}", hex::encode(random)));
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
            (*info).Anonymous.Flags = FILE_RENAME_FLAG_POSIX_SEMANTICS;
        }
        // SAFETY: the handle is open with `DELETE` access, and the buffer is a `FILE_RENAME_INFO` of the size passed.
        let renamed =
            unsafe { SetFileInformationByHandle(self.file.as_raw_handle(), FileRenameInfoEx, info.cast(), size) };
        if renamed != 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INVALID_PARAMETER as i32) {
            return Err(error);
        }
        // SAFETY: as above; the union's `Flags` are cleared, so its `ReplaceIfExists` is false.
        let renamed = unsafe {
            (*info).Anonymous.Flags = 0;
            SetFileInformationByHandle(self.file.as_raw_handle(), FileRenameInfo, info.cast(), size)
        };
        if renamed == 0 {
            return Err(error);
        }
        Ok(())
    }
}

/// [`Host::remove_deleting_name`](crate::host::Host::remove_deleting_name): the name is opened once, for `DELETE`
/// (`FILE_FLAG_OPEN_REPARSE_POINT`: a link is removed, not followed), and marked for deletion through that handle as
/// `std`'s `unlink` does for a read-only file (`posix_delete`), with no second look-up of the name that could find
/// another file. A file that is held (`ERROR_SHARING_VIOLATION`), or whose deletion is pending, is somebody else's to
/// finish. Any other failure, `NotFound` included, is the system's error as it is.
pub(super) fn remove_deleting_name(path: &Path) -> std::io::Result<()> {
    let name = wide(path)?;
    // SAFETY: `name` is NUL-terminated, and a null security descriptor and template are allowed.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    let marked = if handle == INVALID_HANDLE_VALUE {
        Err(Failure::last())
    } else {
        // SAFETY: `handle` is a valid handle that nothing else owns; dropping the file closes it.
        let file = File::from(unsafe { OwnedHandle::from_raw_handle(handle) });
        mark_deleted(
            &file,
            FILE_DISPOSITION_FLAG_POSIX_SEMANTICS | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        )
    };
    match marked {
        Ok(()) => Ok(()),
        Err(failure) if failure.is_another_deleters() => Ok(()),
        Err(failure) => Err(failure.error),
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
        mark_deleted(&self.file, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS).map_err(|failure| failure.error)
    }
}

#[cfg(test)]
#[path = "hold_tests.rs"]
mod hold_tests;
