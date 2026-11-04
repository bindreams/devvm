//! A hold on Windows: the file open with share mode 0.

use std::fs::File;
use std::mem::{offset_of, size_of};
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, ERROR_SHARING_VIOLATION, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_RENAME_INFO, FileDispositionInfo,
    FileDispositionInfoEx, FileRenameInfo, FileRenameInfoEx, OPEN_EXISTING, SetFileInformationByHandle,
};
use windows_sys::Win32::System::WindowsProgramming::FILE_RENAME_FLAG_POSIX_SEMANTICS;

use super::file_id;
use super::wide;
use crate::host::{FileId, Hold, HoldOutcome, deleting_name};
use crate::random;

/// Whether another open of the file refuses this one.
fn is_sharing_violation(error: &std::io::Error) -> bool {
    error.raw_os_error().map(|code| code as u32) == Some(ERROR_SHARING_VIOLATION)
}

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

/// The one open of a file here: `CreateFileW` of `path` (a null security descriptor and template).
fn open(path: &Path, access: u32, share: u32, creation: u32, flags: u32) -> std::io::Result<File> {
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
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `handle` is a valid handle that nothing else owns; dropping the file closes it.
    Ok(File::from(unsafe { OwnedHandle::from_raw_handle(handle) }))
}

/// Marks `file`, open with `DELETE` access, for deletion: `FileDispositionInfoEx` with `FILE_DISPOSITION_FLAG_DELETE`
/// and POSIX semantics, which unlink the name when the file object closes, whoever else has the file open. Where the
/// file system or Windows has no `*Ex` form, `FileDispositionInfo` deletes the file when the last handle closes
/// (`std`'s `win32_delete`).
fn mark_deleted(file: &File) -> std::io::Result<()> {
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
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
    let error = std::io::Error::last_os_error();
    if !is_unsupported(&error) {
        return Err(error);
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
        return Err(std::io::Error::last_os_error());
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
    /// (`AlreadyExists`) is not replaced: the digits are drawn again, with no limit on the draws.
    fn rename_away_with(&self, draw: &mut dyn FnMut() -> [u8; 8]) -> std::io::Result<()> {
        loop {
            let name = wide(&deleting_name(&self.path, draw()))?;
            match self.rename_to(&name) {
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                renamed => return renamed,
            }
        }
    }

    /// Renames the file to `name`, a NUL-terminated wide path, which is not replaced if it exists.
    fn rename_to(&self, name: &[u16]) -> std::io::Result<()> {
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
        let error = std::io::Error::last_os_error();
        if !is_unsupported(&error) {
            return Err(error);
        }
        // SAFETY: as above; the union's `Flags` are cleared, so its `ReplaceIfExists` is false.
        let renamed = unsafe {
            (*info).Anonymous.Flags = 0;
            SetFileInformationByHandle(self.file.as_raw_handle(), FileRenameInfo, info.cast(), size)
        };
        if renamed == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// [`Hold::delete`] with the digits of the deleting name drawn by `draw`.
    fn delete_with(self, draw: &mut dyn FnMut() -> [u8; 8]) -> std::io::Result<()> {
        self.rename_away_with(draw)?;
        mark_deleted(&self.file)
    }
}

/// [`Host::remove_deleting_name`](crate::host::Host::remove_deleting_name): `std::fs::remove_file`, which opens the name
/// once, for `DELETE` (a link is removed, not followed), and deletes through that handle with POSIX semantics, a
/// read-only file included. A file that is held (`ERROR_SHARING_VIOLATION`) is somebody else's to finish: `Ok`. Any
/// other failure, `NotFound` included, is the system's error as it is, as is a name whose deletion is pending, which
/// only somebody deleting the file by hand, or a file system without POSIX deletes, leaves: the next sweep tries again.
pub(super) fn remove_deleting_name(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if is_sharing_violation(&error) => Ok(()),
        removed => removed,
    }
}

/// Opens `path` as a hold: share mode 0 refuses every other open of the file, including `DELETE`, for as long as any
/// handle to this open file exists, the inherited ones too. `DELETE` access is for [`Hold::delete`]. A link at the name
/// is opened itself, not followed.
fn open_hold(path: &Path, creation: u32) -> std::io::Result<File> {
    open(
        path,
        DELETE | FILE_GENERIC_READ | FILE_GENERIC_WRITE,
        0,
        creation,
        FILE_FLAG_OPEN_REPARSE_POINT,
    )
}

/// Creates `path`, which must not exist, and holds it. A name that is taken is `AlreadyExists`.
pub(super) fn new(path: &Path) -> std::io::Result<WindowsHold> {
    let file = open_hold(path, CREATE_NEW)?;
    WindowsHold::of(file, path)
}

/// Holds the file at `path` if nobody else does. A link at the name is refused. A file that is held
/// (`ERROR_SHARING_VIOLATION`) is `Busy`: somebody else has it, and it is not absent.
pub(super) fn existing(path: &Path) -> std::io::Result<HoldOutcome> {
    match open_hold(path, OPEN_EXISTING) {
        Ok(file) if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is a link", path.display()),
        )),
        Ok(file) => Ok(HoldOutcome::Taken(Box::new(WindowsHold::of(file, path)?))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HoldOutcome::Absent),
        Err(error) if is_sharing_violation(&error) => Ok(HoldOutcome::Busy),
        Err(error) => Err(error),
    }
}

impl Hold for WindowsHold {
    fn file(&self) -> &File {
        &self.file
    }

    fn id(&self) -> std::io::Result<FileId> {
        file_id::of_handle(&self.file)
    }

    /// Renames the file away through the handle and marks it for deletion, so no name is looked up again. No name is
    /// left delete-pending by it: the hold is share mode 0, so nobody else can mark the file, and the rename comes
    /// before the mark. A handle to
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
