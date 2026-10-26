//! What the fake VM reports about itself on Windows.

use windows_sys::Win32::System::Console::GetConsoleProcessList;
use windows_sys::Win32::System::JobObjects::{
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    QueryInformationJobObject,
};

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
