//! What the fake VM reports about itself on Windows.

use std::os::windows::io::{FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, GetLastError, HANDLE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
use windows_sys::Win32::System::JobObjects::{
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    QueryInformationJobObject,
};
use windows_sys::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};

/// Whether the job this process is in kills its members when its last handle closes.
pub fn kill_on_close() -> &'static str {
    // SAFETY: a zeroed limit-information struct is valid, and the size passed is its own.
    let (queried, flags) = unsafe {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        let queried = QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut info).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        );
        (queried != 0, info.BasicLimitInformation.LimitFlags)
    };
    match (queried, flags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0) {
        (false, _) => "n/a",
        (true, true) => "true",
        (true, false) => "false",
    }
}

/// How many processes share this one's console; 0 when it has none.
pub fn console_processes() -> String {
    let mut list = [0u32; 1];
    // SAFETY: the list is one element long, as passed.
    let count = unsafe { GetConsoleProcessList(list.as_mut_ptr(), 1) };
    count.to_string()
}

/// `window` when this process has a console window, `none` when it has a console without one or no console.
pub fn console_window() -> &'static str {
    // SAFETY: no arguments; returns a handle or null.
    if unsafe { GetConsoleWindow() }.is_null() {
        "none"
    } else {
        "window"
    }
}

/// Holds an inheritable pipe's write end while `spawn` runs, then closes it and reports the read end: `broken` when
/// nothing else holds the write end, `open` when something does, as a process that inherited it would. `spawn`'s
/// result lives until the report, so a process it started is still there.
pub fn pipe_after<T>(spawn: impl FnOnce() -> T) -> &'static str {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (mut read, mut write): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: both out-pointers are valid, and the attributes are a `SECURITY_ATTRIBUTES` of the size it names.
    let created = unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) };
    assert_ne!(created, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: `CreatePipe` returned both handles, and nothing else owns them.
    let (read, write) = unsafe { (OwnedHandle::from_raw_handle(read), OwnedHandle::from_raw_handle(write)) };
    let spawned = spawn();
    drop(write);
    // SAFETY: the handle is a pipe's read end; no buffer is asked for.
    let peeked = unsafe {
        use std::os::windows::io::AsRawHandle;
        PeekNamedPipe(
            read.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    // SAFETY: no arguments.
    let error = unsafe { GetLastError() };
    drop(spawned);
    match (peeked, error) {
        (0, ERROR_BROKEN_PIPE) => "broken",
        (0, other) => panic!("peeking the pipe failed with {other}"),
        _ => "open",
    }
}
