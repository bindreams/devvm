//! Waking a thread that is blocked on an AF_UNIX socket.

use std::os::windows::io::AsRawSocket;

use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError, HANDLE};
use windows_sys::Win32::System::IO::CancelIoEx;

use crate::backend::qemu::uds::UnixStream;

#[cfg(test)]
#[path = "interrupt_tests.rs"]
mod interrupt_tests;

/// Makes reads on `stream`'s socket non-blocking and cancels the one in progress, if any. Winsock's `shutdown` does not
/// wake a blocked `recv` and discards unread data (measured), so neither is used. The cancelled call fails with
/// `WSAEINTR`; every later read returns the bytes already received, then ends the stream or reports `WouldBlock`.
pub(super) fn interrupt(stream: &UnixStream) -> std::io::Result<()> {
    stream.set_nonblocking(true)?;
    // SAFETY: `stream` owns the socket and is borrowed for the whole call, so the handle is valid. A null overlapped
    // pointer cancels every pending operation on the handle.
    let cancelled = unsafe { CancelIoEx(stream.as_raw_socket() as HANDLE, std::ptr::null()) } != 0;
    // SAFETY: `GetLastError` reads this thread's error code.
    cancelled_or(cancelled, || unsafe { GetLastError() })
}

/// The outcome of a `CancelIoEx`: it fails with `ERROR_NOT_FOUND` when nothing was pending, which is a reader that is
/// not blocked and so needs no waking. Any other failure leaves a blocked reader asleep.
fn cancelled_or(cancelled: bool, last_error: impl FnOnce() -> u32) -> std::io::Result<()> {
    if cancelled {
        return Ok(());
    }
    match last_error() {
        ERROR_NOT_FOUND => Ok(()),
        other => Err(std::io::Error::from_raw_os_error(other as i32)),
    }
}
