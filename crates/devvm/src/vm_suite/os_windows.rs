//! What the tests ask Windows about a process they did not write.

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_FILES, GetLastError, HANDLE, LPARAM, STILL_ACTIVE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId};

/// A handle that reports a process's exit code after the process is gone (a PID alone cannot).
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

/// The processes that have `pid` as their parent in one snapshot of the process list, each as the list names it: image
/// name, PID and parent PID. A console program has a `conhost.exe` child; a detached one has none.
///
/// Panics when the list cannot be read to its end.
pub fn children_of(pid: u32) -> Vec<String> {
    /// Whether a `Process32FirstW` or `Process32NextW` result is an entry. A zero is the end of the list only when the
    /// last error says so.
    fn entry_listed(result: i32) -> bool {
        if result != 0 {
            return true;
        }
        // SAFETY: reads the calling thread's last error, which the failed call just set.
        let code = unsafe { GetLastError() };
        assert_eq!(
            code, ERROR_NO_MORE_FILES,
            "listing the processes failed with error {code}"
        );
        false
    }

    // SAFETY: a snapshot of the process list.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    assert!(
        !snapshot.is_null() && snapshot as isize != -1,
        "cannot snapshot the process list"
    );
    // SAFETY: an all-zero entry with its size set is what the API expects.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut children = Vec::new();
    // SAFETY: `snapshot` is open and `entry` is valid.
    let mut more = entry_listed(unsafe { Process32FirstW(snapshot, &mut entry) });
    while more {
        if entry.th32ParentProcessID == pid {
            let image_len = entry
                .szExeFile
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(entry.szExeFile.len());
            let image = String::from_utf16_lossy(&entry.szExeFile[..image_len]);
            children.push(format!(
                "{image} (pid {}, parent pid {})",
                entry.th32ProcessID, entry.th32ParentProcessID
            ));
        }
        // SAFETY: as above.
        more = entry_listed(unsafe { Process32NextW(snapshot, &mut entry) });
    }
    // SAFETY: closed once.
    unsafe { CloseHandle(snapshot) };
    children
}
