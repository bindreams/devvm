//! What the tests ask Windows about a process they did not write.

use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, LPARAM, STILL_ACTIVE, UNICODE_STRING};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId};

/// A handle that reports a process's exit code after the process is gone (a PID alone cannot), and what the OS keeps of
/// it while it runs.
pub struct Watch(usize);

impl Watch {
    /// Opens `pid` while it runs. `None` when the process cannot be opened.
    pub fn new(pid: u32) -> Option<Watch> {
        // SAFETY: plain query access to a process by id.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        (!handle.is_null()).then_some(Watch(handle as usize))
    }

    /// The exit code, once the process has exited.
    pub fn exit_code(&self) -> Option<u32> {
        let mut code = 0u32;
        // SAFETY: the handle is open for as long as `self` lives; `code` is a valid out pointer.
        let ok = unsafe { GetExitCodeProcess(self.0 as HANDLE, &mut code) };
        (ok != 0 && code != STILL_ACTIVE as u32).then_some(code)
    }

    /// When the process was created, in 100 ns ticks since 1601.
    fn created(&self) -> Option<u64> {
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: the handle is open; the four out pointers are valid.
        let ok = unsafe { GetProcessTimes(self.0 as HANDLE, &mut created, &mut exited, &mut kernel, &mut user) };
        (ok != 0).then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }

    /// The command line the process was started with, as the OS keeps it.
    fn command_line(&self) -> Option<String> {
        let handle = self.0 as HANDLE;
        let mut needed = 0u32;
        // SAFETY: a size query: no buffer.
        unsafe {
            NtQueryInformationProcess(
                handle,
                ProcessCommandLineInformation,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        // In 8-byte units, so the buffer is aligned for the `UNICODE_STRING` the OS writes at its start.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        let size = (buffer.len() * 8) as u32;
        // SAFETY: the buffer is `size` bytes; `needed` is a valid out pointer.
        let status = unsafe {
            NtQueryInformationProcess(
                handle,
                ProcessCommandLineInformation,
                buffer.as_mut_ptr().cast(),
                size,
                &mut needed,
            )
        };
        if status < 0 {
            return None;
        }
        // SAFETY: on success the buffer starts with a `UNICODE_STRING` whose text lies inside the buffer.
        let text = unsafe { &*buffer.as_ptr().cast::<UNICODE_STRING>() };
        // SAFETY: `Length` bytes of UTF-16 at `Buffer`, inside `buffer`, which is alive and unchanged.
        let units = unsafe { std::slice::from_raw_parts(text.Buffer, usize::from(text.Length) / 2) };
        Some(String::from_utf16_lossy(units))
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: the handle came from `OpenProcess` and is closed once.
        unsafe { CloseHandle(self.0 as HANDLE) };
    }
}

/// How many top-level windows belong to `pid`.
pub fn windows_of(pid: u32) -> usize {
    struct Count {
        pid: u32,
        found: usize,
    }
    unsafe extern "system" fn visit(window: windows_sys::Win32::Foundation::HWND, state: LPARAM) -> i32 {
        // SAFETY: `state` is the address of the `Count` below, alive for the whole `EnumWindows` call.
        let count = unsafe { &mut *(state as *mut Count) };
        let mut owner = 0u32;
        // SAFETY: `window` comes from the enumeration; `owner` is a valid out pointer.
        unsafe { GetWindowThreadProcessId(window, &mut owner) };
        if owner == count.pid {
            count.found += 1;
        }
        1
    }
    let mut count = Count { pid, found: 0 };
    // SAFETY: `visit` only reads `count`, which outlives the call.
    unsafe { EnumWindows(Some(visit), &mut count as *mut Count as LPARAM) };
    count.found
}

/// The running processes that have `pid` as their parent, each described: image name, PID, command line, and when it
/// was created relative to `pid`'s process. A console program has a `conhost.exe` child; a detached one has none. The
/// parent is whatever the process list says, which a reused PID can make a process that never started the child: a child
/// created before its parent is that.
pub fn children_of(pid: u32) -> Vec<String> {
    // SAFETY: a snapshot of the process list.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    assert!(
        !snapshot.is_null() && snapshot as isize != -1,
        "cannot snapshot the process list"
    );
    // SAFETY: an all-zero entry with its size set is what the API expects.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let parent_created = Watch::new(pid).and_then(|parent| parent.created());
    let mut children = Vec::new();
    // SAFETY: `snapshot` is open and `entry` is valid.
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32ParentProcessID == pid {
            let image_len = entry
                .szExeFile
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(entry.szExeFile.len());
            let image = String::from_utf16_lossy(&entry.szExeFile[..image_len]);
            children.push(describe_child(&image, entry.th32ProcessID, parent_created));
        }
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    // SAFETY: closed once.
    unsafe { CloseHandle(snapshot) };
    children
}

fn describe_child(image: &str, pid: u32, parent_created: Option<u64>) -> String {
    let child = Watch::new(pid);
    let created = child.as_ref().and_then(Watch::created);
    let when = match (created, parent_created) {
        (Some(child), Some(parent)) if child >= parent => {
            format!("created {} ms after its parent", (child - parent) / 10_000)
        }
        (Some(child), Some(parent)) => format!("created {} ms before its parent", (parent - child) / 10_000),
        _ => "creation time unreadable".to_owned(),
    };
    let command_line = child
        .as_ref()
        .and_then(Watch::command_line)
        .unwrap_or_else(|| "unreadable".to_owned());
    format!("{image} (pid {pid}, {when}, command line: {command_line})")
}
