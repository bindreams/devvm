//! A manual-reset event, which `WaitForMultipleObjects` waits on like a process handle.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::Arc;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};

use crate::host::ProcessWatch;
use crate::host::windows::WindowsWatch;

pub struct ManualExit {
    event: OwnedHandle,
    watch: Arc<dyn ProcessWatch>,
}

impl ManualExit {
    pub fn new() -> ManualExit {
        // SAFETY: no security attributes, a manual-reset event, not signalled, unnamed.
        let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!raw.is_null(), "{}", std::io::Error::last_os_error());
        // SAFETY: `raw` is a new event handle that nothing else owns.
        let event = unsafe { OwnedHandle::from_raw_handle(raw as _) };
        let watch = Arc::new(WindowsWatch::from_handle(event.try_clone().unwrap()));
        ManualExit { event, watch }
    }

    /// The watch that reports the exit once `exit` has been called.
    pub fn watch(&self) -> Arc<dyn ProcessWatch> {
        self.watch.clone()
    }

    /// The process exits. Calling it again does nothing.
    pub fn exit(&self) {
        // SAFETY: `event` is a live event handle.
        assert!(unsafe { SetEvent(self.event.as_raw_handle() as HANDLE) } != 0);
    }
}
