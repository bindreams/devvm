//! A hold on Windows: the file open with share mode 0.

use std::fs::File;
use std::mem::{offset_of, size_of};
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, ERROR_SHARING_VIOLATION,
    INVALID_HANDLE_VALUE, NTSTATUS, STATUS_DELETE_PENDING, STATUS_SUCCESS,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO,
    FILE_DISPOSITION_INFO_EX, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_RENAME_INFO,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo, FileDispositionInfoEx, FileRenameInfo,
    FileRenameInfoEx, OPEN_EXISTING, SetFileInformationByHandle,
};
use windows_sys::Win32::System::WindowsProgramming::FILE_RENAME_FLAG_POSIX_SEMANTICS;

use super::file_id;
use super::wide;
use crate::host::{FileId, Hold, HoldOutcome, deleting_name};
use crate::random;

/// Whether `error` says the file system does not do the `*Ex` form of a call: it gates both fallbacks here, the
/// disposition ([`mark_deleted`]) and the rename (`rename_to`). `std`'s `File::delete`
/// (`library/std/src/sys/fs/windows.rs`) falls back from `posix_delete` to `win32_delete` on exactly these three:
/// `ERROR_INVALID_PARAMETER` and `ERROR_NOT_SUPPORTED`, which `posix_delete` documents (FAT32, exFAT, SMB, Windows
/// before 10 1607), and `ERROR_INVALID_FUNCTION`. `std`'s `rename` has no fallback of its own (`MoveFileExW`, then
/// `FileRenameInfoEx` for a read-only file), so the rename here takes the delete's three.
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

    /// Whether a file's deletion is pending: `ERROR_ACCESS_DENIED` like a real refusal, and the status tells them
    /// apart.
    fn is_delete_pending(&self) -> bool {
        self.error.raw_os_error().map(|code| code as u32) == Some(ERROR_ACCESS_DENIED)
            && self.status == STATUS_DELETE_PENDING
    }

    /// Whether another open of the file refuses this one (`ERROR_SHARING_VIOLATION`).
    fn is_sharing_violation(&self) -> bool {
        self.error.raw_os_error().map(|code| code as u32) == Some(ERROR_SHARING_VIOLATION)
    }

    /// The failure as the caller gets it: a name whose deletion is pending is `AlreadyExists`, as one that a file has
    /// (the system answers `ERROR_ACCESS_DENIED`, which no caller can tell from a refusal); any other is the system's
    /// error as it is.
    fn into_error(self) -> std::io::Error {
        if self.is_delete_pending() {
            return std::io::Error::new(std::io::ErrorKind::AlreadyExists, "deletion of the file is pending");
        }
        self.error
    }
}

/// An error that no system call of ours ended with, such as a path that has no wide form: it has no status.
impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Failure {
        Failure {
            error,
            status: STATUS_SUCCESS,
        }
    }
}

/// The one open of a file here: `CreateFileW` of `path` (a null security descriptor and template), whose failure keeps
/// the `NTSTATUS` it ended with.
fn open(path: &Path, access: u32, share: u32, creation: u32, flags: u32) -> Result<File, Failure> {
    let name = wide(path)?;
    // SAFETY: `name` is NUL-terminated, and a null security descriptor and template are allowed.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            share,
            std::ptr::null(),
            creation,
            flags,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(Failure::last());
    }
    // SAFETY: `handle` is a valid handle that nothing else owns; dropping the file closes it.
    Ok(File::from(unsafe { OwnedHandle::from_raw_handle(handle) }))
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

    /// Renames the file through its handle to its deleting name ([`deleting_name`]) for the digits `draw` gives, with
    /// POSIX semantics, which let the rename go through while somebody has the file open (`FileRenameInfoEx`); where
    /// the file system has none, `FileRenameInfo`. The name is drawn for this delete alone, so it is never one that
    /// another delete, a duplicate of another file or a scanner still has. The name the file had is free from here on,
    /// though the file keeps existing until every handle to it is closed.
    ///
    /// Falls back where [`is_unsupported`], and returns the fallback's error when it fails too. A name that is taken
    /// (`AlreadyExists`: a file is there, or its deletion is pending, [`Failure::into_error`]) is not replaced: the
    /// digits are drawn again, with no limit on the draws.
    ///
    /// A rename of a file whose own deletion is pending is refused as a pending target is, and would draw forever. It
    /// cannot happen here: the hold is share mode 0, so only [`mark_deleted`] through this handle can mark the file,
    /// and `delete_with` does that last, after the rename.
    fn rename_away_with(&self, draw: &mut dyn FnMut() -> [u8; 8]) -> std::io::Result<()> {
        loop {
            let name = wide(&deleting_name(&self.path, draw()))?;
            match self.rename_to(&name).map_err(Failure::into_error) {
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                renamed => return renamed,
            }
        }
    }

    /// Renames the file to `name`, a NUL-terminated wide path, which is not replaced if it exists.
    fn rename_to(&self, name: &[u16]) -> Result<(), Failure> {
        let name = &name[..name.len() - 1]; // the NUL: the name's length is stated, not terminated
        let name_bytes = std::mem::size_of_val(name);
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
        let failure = Failure::last();
        if !is_unsupported(&failure.error) {
            return Err(failure);
        }
        // SAFETY: as above; the union's `Flags` are cleared, so its `ReplaceIfExists` is false.
        let renamed = unsafe {
            (*info).Anonymous.Flags = 0;
            SetFileInformationByHandle(self.file.as_raw_handle(), FileRenameInfo, info.cast(), size)
        };
        if renamed == 0 {
            return Err(Failure::last());
        }
        Ok(())
    }

    /// [`Hold::delete`] with the digits of the deleting name drawn by `draw`.
    fn delete_with(self, draw: &mut dyn FnMut() -> [u8; 8]) -> std::io::Result<()> {
        self.rename_away_with(draw)?;
        mark_deleted(&self.file, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS).map_err(|failure| failure.error)
    }
}

/// [`Host::remove_deleting_name`](crate::host::Host::remove_deleting_name): the name is opened once, for `DELETE`
/// (`FILE_FLAG_OPEN_REPARSE_POINT`: a link is removed, not followed), and marked for deletion through that handle as
/// `std`'s `unlink` does for a read-only file (`posix_delete`), with no second look-up of the name that could find
/// another file. A file that is held (`ERROR_SHARING_VIOLATION`), or whose deletion is pending, is somebody else's to
/// finish. Any other failure, `NotFound` included, is the system's error as it is.
pub(super) fn remove_deleting_name(path: &Path) -> std::io::Result<()> {
    let marked = open(
        path,
        DELETE,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        OPEN_EXISTING,
        FILE_FLAG_OPEN_REPARSE_POINT,
    )
    .and_then(|file| {
        mark_deleted(
            &file,
            FILE_DISPOSITION_FLAG_POSIX_SEMANTICS | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
        )
    });
    match marked {
        Ok(()) => Ok(()),
        Err(failure) if failure.is_sharing_violation() || failure.is_delete_pending() => Ok(()),
        Err(failure) => Err(failure.error),
    }
}

/// Opens `path` as a hold: share mode 0 refuses every other open of the file, including `DELETE`, for as long as any
/// handle to this open file exists, the inherited ones too. `DELETE` access is for [`Hold::delete`]. A link at the name
/// is opened itself, not followed.
fn open_hold(path: &Path, creation: u32) -> Result<File, Failure> {
    open(
        path,
        DELETE | FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        0,
        creation,
        FILE_FLAG_OPEN_REPARSE_POINT,
    )
}

/// Creates `path`, which must not exist, and holds it. A name that is taken is `AlreadyExists`, one whose deletion is
/// pending included.
pub(super) fn new(path: &Path) -> std::io::Result<WindowsHold> {
    let file = open_hold(path, CREATE_NEW).map_err(Failure::into_error)?;
    WindowsHold::of(file, path)
}

/// Holds the file at `path` if nobody else does. A link at the name is refused. A file that is held, or whose deletion
/// is pending, is `Busy`: somebody else has it, or is deleting it, and it is not absent.
pub(super) fn existing(path: &Path) -> std::io::Result<HoldOutcome> {
    match open_hold(path, OPEN_EXISTING) {
        Ok(file) if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is a link", path.display()),
        )),
        Ok(file) => Ok(HoldOutcome::Taken(Box::new(WindowsHold::of(file, path)?))),
        Err(failure) if failure.error.kind() == std::io::ErrorKind::NotFound => Ok(HoldOutcome::Absent),
        Err(failure) if failure.is_sharing_violation() || failure.is_delete_pending() => Ok(HoldOutcome::Busy),
        Err(failure) => Err(failure.error),
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
        self.delete_with(&mut random::bytes::<8>)
    }
}

#[cfg(test)]
#[path = "hold_tests.rs"]
mod hold_tests;
