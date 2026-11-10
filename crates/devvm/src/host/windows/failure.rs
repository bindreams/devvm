//! `CreateFileW` and the failure of a system call, which keeps the `NTSTATUS` it ended with.

use std::fs::File;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, ERROR_SHARING_VIOLATION,
    INVALID_HANDLE_VALUE, NTSTATUS, STATUS_DELETE_PENDING, STATUS_SUCCESS,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;

use super::wide;

// `windows-sys` has no binding for it.
#[link(name = "ntdll")]
unsafe extern "system" {
    /// The `NTSTATUS` the calling thread's last system call ended with, which Win32 maps to an error code that can
    /// stand for several statuses.
    fn RtlGetLastNtStatus() -> NTSTATUS;
}

/// A failed system call: its Win32 error and the `NTSTATUS` it ended with (`STATUS_SUCCESS` for an error that no
/// system call ended with, such as a path that has no wide form).
pub(super) struct Failure {
    pub(super) error: std::io::Error,
    status: NTSTATUS,
}

impl Failure {
    /// The failure of the calling thread's last system call, which must be the one that failed: both are read from the
    /// thread's last call, so nothing may call the system in between.
    pub(super) fn last() -> Failure {
        let error = std::io::Error::last_os_error();
        // SAFETY: reads a thread-local status; no argument.
        let status = unsafe { RtlGetLastNtStatus() };
        Failure { error, status }
    }

    /// Whether a file's deletion is pending: `ERROR_ACCESS_DENIED` like a real refusal, and the status tells them
    /// apart. The error is checked as well: a call that fails before the kernel (an empty path, a bad buffer length)
    /// leaves the status of an earlier call.
    pub(super) fn is_delete_pending(&self) -> bool {
        self.error.raw_os_error().map(|code| code as u32) == Some(ERROR_ACCESS_DENIED)
            && self.status == STATUS_DELETE_PENDING
    }

    /// Whether the file system or Windows has no `*Ex` form of the call; gates the disposition (`mark_deleted`) and
    /// rename (`rename_to`) fallbacks. Matches the three errors std's `File::delete` falls back on.
    pub(super) fn is_unsupported(&self) -> bool {
        matches!(
            self.error.raw_os_error().map(|code| code as u32),
            Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION)
        )
    }

    /// Whether another open of the file refuses this one (`ERROR_SHARING_VIOLATION`).
    pub(super) fn is_sharing_violation(&self) -> bool {
        self.error.raw_os_error().map(|code| code as u32) == Some(ERROR_SHARING_VIOLATION)
    }
}

/// The system's error as it is, status dropped.
impl From<Failure> for std::io::Error {
    fn from(failure: Failure) -> std::io::Error {
        failure.error
    }
}

/// `CreateFileW` of `path` (a null template), whose failure keeps the `NTSTATUS` it ended with. A file that `creation`
/// makes gets the descriptor in `security`, or the inherited one when it is `None`.
pub(super) fn open(
    path: &Path,
    access: u32,
    share: u32,
    creation: u32,
    flags: u32,
    security: Option<&SECURITY_ATTRIBUTES>,
) -> Result<File, Failure> {
    let name = wide(path).map_err(|error| Failure {
        error,
        status: STATUS_SUCCESS,
    })?;
    let security = security.map_or(std::ptr::null(), std::ptr::from_ref);
    // SAFETY: `name` is NUL-terminated, a null template is allowed, and `security` is null or borrowed for the call.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            share,
            security,
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
