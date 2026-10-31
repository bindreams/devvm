//! Watching a process through a handle on it.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::Arc;

use cosca::identity::{Liveness, ProcessId};
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

use super::link::EventLink;
use crate::backend::qemu::uds::UnixStream;
use crate::host::watch::{Link, ProcessWatch, RETRY};
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "watch_tests.rs"]
mod watch_tests;

/// A process, or anything else `WaitForMultipleObjects` waits on, as a handle that signals when it ends.
pub(crate) struct WindowsWatch {
    /// `None`: the process had exited before the handle was opened.
    handle: Option<Arc<OwnedHandle>>,
}

impl WindowsWatch {
    /// Opens the process `id` names. The handle pins that process, so a recycled PID cannot be mistaken for it: the
    /// identity is checked after the open, and a process that is alive then held its PID all along.
    pub(super) fn open(id: ProcessId) -> Result<WindowsWatch> {
        let pid = id.pid();
        // SAFETY: plain `OpenProcess`; no pointers.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if raw.is_null() {
            let error = std::io::Error::last_os_error();
            // No process has this PID any more.
            if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                return Ok(WindowsWatch { handle: None });
            }
            return Err(Error::io(
                ErrorKind::Io,
                format!("cannot open the VM process {pid} to wait for its exit"),
                error,
            ));
        }
        // SAFETY: `raw` is a new handle that nothing else owns.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw as _) };
        match cosca::Process::from_id(id).is_alive() {
            Liveness::Alive => Ok(WindowsWatch::from_handle(handle)),
            Liveness::Dead => Ok(WindowsWatch { handle: None }),
            Liveness::Unknown => Err(Error::new(
                ErrorKind::Io,
                format!("cannot tell whether the VM process {pid} is running: the OS refused the query"),
            )),
        }
    }

    /// A watch of `handle`, which signals when the process ends.
    pub(crate) fn from_handle(handle: OwnedHandle) -> WindowsWatch {
        WindowsWatch {
            handle: Some(Arc::new(handle)),
        }
    }

    /// Whether the handle signals within `millis`.
    fn signalled_within(&self, millis: u32) -> Result<bool> {
        let Some(handle) = &self.handle else {
            return Ok(true);
        };
        // SAFETY: `handle` is open for as long as `self` is.
        match unsafe { WaitForSingleObject(handle.as_raw_handle() as HANDLE, millis) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(Error::io(
                ErrorKind::Io,
                "cannot wait for the VM process",
                std::io::Error::last_os_error(),
            )),
        }
    }
}

impl ProcessWatch for WindowsWatch {
    fn has_exited(&self) -> Result<bool> {
        self.signalled_within(0)
    }

    fn pause(&self) -> Result<bool> {
        self.signalled_within(RETRY.as_millis() as u32)
    }

    fn link(&self, stream: UnixStream) -> std::io::Result<Box<dyn Link>> {
        Ok(Box::new(EventLink::new(stream, self.handle.clone())?))
    }
}
