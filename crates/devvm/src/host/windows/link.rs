//! A connection whose reader waits for the socket and for the process together.
//!
//! Windows can lose the end of an AF_UNIX stream: a read blocked on a socket whose peer died may never wake. So the
//! socket is non-blocking and every wait is a `WaitForMultipleObjects` on two kernel objects, the socket's event
//! (`WSAEventSelect`) and the process: the thread that reads is the thread that waits. Nothing is cancelled or shut
//! down from another thread, so no byte is discarded, and a wakeup cannot be lost, because both objects stay
//! signalled until the reader has looked.

use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, AsRawSocket, OwnedHandle};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Networking::WinSock::{
    FD_CLOSE, FD_READ, FD_WRITE, WSACloseEvent, WSACreateEvent, WSAEVENT, WSAEnumNetworkEvents, WSAEventSelect,
    WSAGetLastError, WSANETWORKEVENTS,
};
use windows_sys::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};

use crate::backend::qemu::uds::UnixStream;

#[cfg(test)]
#[path = "link_tests.rs"]
mod link_tests;

pub(super) struct EventLink {
    stream: UnixStream,
    /// Signalled when the socket becomes readable or writable or closes.
    event: WSAEVENT,
    /// `None`: the process had exited before the link was made.
    process: Option<Arc<OwnedHandle>>,
    /// The process has exited. What is left to read is what the socket holds.
    exited: bool,
}

impl EventLink {
    pub(super) fn new(stream: UnixStream, process: Option<Arc<OwnedHandle>>) -> std::io::Result<EventLink> {
        // SAFETY: no arguments.
        let event = unsafe { WSACreateEvent() };
        if event == 0 {
            return Err(wsa_error());
        }
        // SAFETY: `stream` owns the socket and `event` is the event just made. This also makes the socket non-blocking.
        let selected = unsafe {
            WSAEventSelect(
                stream.as_raw_socket() as _,
                event,
                (FD_READ | FD_WRITE | FD_CLOSE) as i32,
            )
        };
        if selected != 0 {
            let error = wsa_error();
            // SAFETY: `event` is open and nothing else has it.
            unsafe { WSACloseEvent(event) };
            return Err(error);
        }
        Ok(EventLink {
            stream,
            event,
            exited: process.is_none(),
            process,
        })
    }

    /// Returns when the socket has something to report or the process has exited (`self.exited`). Spurious returns are
    /// fine: the caller tries again.
    fn wait(&mut self) -> std::io::Result<()> {
        let process = self.process.as_ref().expect("a link to an exited process never waits");
        let handles = [self.event as HANDLE, process.as_raw_handle() as HANDLE];
        // SAFETY: both handles are open; the process handle is owned by the `Arc` held here. The socket is listed first
        // so that, when both are signalled, its bytes are read before the exit is acted on.
        match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) } {
            WAIT_OBJECT_0 => self.reset_event(),
            n if n == WAIT_OBJECT_0 + 1 => {
                self.exited = true;
                Ok(())
            }
            _ => Err(std::io::Error::last_os_error()),
        }
    }

    /// Clears the event before the caller tries the socket again, so that what arrives from now on signals it again and
    /// what is already there is found by the try.
    fn reset_event(&self) -> std::io::Result<()> {
        let mut events = WSANETWORKEVENTS {
            lNetworkEvents: 0,
            iErrorCode: [0; 10],
        };
        // SAFETY: the socket and event are open; `events` is a valid out-parameter.
        match unsafe { WSAEnumNetworkEvents(self.stream.as_raw_socket() as _, self.event, &mut events) } {
            0 => Ok(()),
            _ => Err(wsa_error()),
        }
    }
}

impl Read for EventLink {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.stream.read(buf) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                other => return other,
            }
            // The socket is empty. After the exit nothing more comes.
            if self.exited {
                return Ok(0);
            }
            self.wait()?;
        }
    }
}

impl Write for EventLink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        loop {
            match self.stream.write(buf) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                other => return other,
            }
            if self.exited {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.wait()?;
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

impl Drop for EventLink {
    fn drop(&mut self) {
        // SAFETY: the socket is still open (`stream` drops after this) and the event is ours.
        unsafe {
            WSAEventSelect(self.stream.as_raw_socket() as _, self.event, 0);
            WSACloseEvent(self.event);
        }
    }
}

fn wsa_error() -> std::io::Error {
    // SAFETY: reads this thread's error code.
    std::io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}
