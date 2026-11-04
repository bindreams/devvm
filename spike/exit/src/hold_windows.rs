//! The hold on Windows: a file opened share-mode 0, the handle made inheritable and passed to a child.
use std::fs::File;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::process::{Child, Command};

use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const ERROR_SHARING_VIOLATION: i32 = 32;

/// Owns the handle; dropping it closes the parent's copy.
pub struct Hold(#[allow(dead_code)] File);

impl Hold {
    pub fn create(path: &Path) -> Self {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(path)
            .unwrap();
        let ok = unsafe { SetHandleInformation(file.as_raw_handle(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        assert_ne!(ok, 0);
        Self(file)
    }

    /// std's `Command` spawns with bInheritHandles = TRUE, so the inheritable handle is passed on.
    pub fn inherit_into(&self, _command: &mut Command) {}
}

/// Whether the hold could be taken (open, share-mode 0); released again at once.
pub fn try_take(path: &Path) -> bool {
    match File::options().read(true).write(true).share_mode(0).open(path) {
        Ok(_) => true,
        Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => false,
        Err(e) => panic!("probe: {e}"),
    }
}

/// WaitForSingleObject(handle, 0).
pub fn has_exited(child: &mut Child) -> bool {
    let wait = unsafe { WaitForSingleObject(child.as_raw_handle(), 0) };
    wait == WAIT_OBJECT_0
}
