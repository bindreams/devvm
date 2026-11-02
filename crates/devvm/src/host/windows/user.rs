//! The account the process runs as, and the folder Windows keeps machine-wide state in.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
use std::path::PathBuf;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

/// An error that says which call failed.
fn call_failed(call: &str, source: io::Error) -> io::Error {
    io::Error::new(source.kind(), format!("{call} failed ({source})"))
}

/// `S-1-5-...` for `psid`.
pub(super) fn sid_string(psid: PSID) -> io::Result<String> {
    let mut raw: *mut u16 = null_mut();
    // SAFETY: `psid` is a valid SID; on success `raw` is a NUL-terminated string that `LocalFree` frees.
    if unsafe { ConvertSidToStringSidW(psid, &mut raw) } == 0 {
        return Err(call_failed("ConvertSidToStringSidW", io::Error::last_os_error()));
    }
    let text = wide_to_string(raw);
    // SAFETY: `raw` came from `ConvertSidToStringSidW`, which documents `LocalFree` for it.
    unsafe { LocalFree(raw.cast()) };
    Ok(text)
}

/// The text of the NUL-terminated UTF-16 string `raw`.
fn wide_to_string(raw: *const u16) -> String {
    // SAFETY: `raw` is NUL-terminated, so scanning up to the NUL stays inside the allocation.
    let units = unsafe {
        let mut len = 0;
        while *raw.add(len) != 0 {
            len += 1;
        }
        std::slice::from_raw_parts(raw, len)
    };
    OsString::from_wide(units).to_string_lossy().into_owned()
}

/// The process token's user, as `TOKEN_USER` in 8-byte-aligned storage. The SID points into the same buffer.
pub(super) struct TokenUserBuffer(Vec<u64>);

impl TokenUserBuffer {
    pub(super) fn read() -> io::Result<TokenUserBuffer> {
        let mut raw: HANDLE = null_mut();
        // SAFETY: `raw` is a valid out-pointer; the pseudo-handle from `GetCurrentProcess` needs no closing.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
            return Err(call_failed("OpenProcessToken", io::Error::last_os_error()));
        }
        // SAFETY: `OpenProcessToken` succeeded, so `raw` is an open handle that this value owns.
        let _token = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
        let mut buffer: Vec<u64> = Vec::new();
        let mut bytes = 0u32;
        loop {
            let pointer = if buffer.is_empty() {
                null_mut()
            } else {
                buffer.as_mut_ptr()
            };
            // SAFETY: `buffer` has `buffer.len() * 8` writable bytes, which is the length passed.
            let read =
                unsafe { GetTokenInformation(raw, TokenUser, pointer.cast(), (buffer.len() * 8) as u32, &mut bytes) };
            if read != 0 {
                return Ok(TokenUserBuffer(buffer));
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
                return Err(call_failed("GetTokenInformation", error));
            }
            buffer.resize((bytes as usize).div_ceil(8), 0);
        }
    }

    /// The user's SID, valid while `self` lives.
    pub(super) fn sid(&self) -> PSID {
        // SAFETY: the buffer holds the `TOKEN_USER` that `GetTokenInformation` wrote.
        unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

/// The SID string of the user the process token names.
pub(super) fn current_sid() -> io::Result<String> {
    sid_string(TokenUserBuffer::read()?.sid())
}

/// `C:\ProgramData`, from the known folder and not from the environment.
pub(super) fn program_data() -> io::Result<PathBuf> {
    let mut raw: *mut u16 = null_mut();
    // SAFETY: a null token means the current user; on success `raw` is a NUL-terminated string that `CoTaskMemFree` frees.
    let status = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DEFAULT as u32, null_mut(), &mut raw) };
    if status != 0 {
        // SAFETY: the documentation asks for the buffer to be freed even when the call failed.
        unsafe { CoTaskMemFree(raw.cast()) };
        return Err(call_failed(
            "SHGetKnownFolderPath(FOLDERID_ProgramData)",
            io::Error::from_raw_os_error(status),
        ));
    }
    let text = wide_to_string(raw);
    // SAFETY: `raw` came from `SHGetKnownFolderPath`, which documents `CoTaskMemFree` for it.
    unsafe { CoTaskMemFree(raw.cast()) };
    Ok(PathBuf::from(text))
}
