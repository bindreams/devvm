//! Waking a thread that is blocked on an AF_UNIX socket.

use std::net::Shutdown;
use std::os::windows::io::AsRawSocket;

use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError, HANDLE};
use windows_sys::Win32::System::IO::CancelIoEx;

use crate::backend::qemu::uds::UnixStream;

#[cfg(test)]
#[path = "interrupt_tests.rs"]
mod interrupt_tests;

/// Makes reads on `stream`'s socket non-blocking and cancels the one in progress, if any. Winsock's `shutdown` does not
/// wake a blocked `recv` and discards unread data (measured), so it is only the fallback: the cancelled call fails with
/// `WSAEINTR`; every later read returns the bytes already received, then ends the stream or reports `WouldBlock`.
///
/// When the cancel fails, the connection is shut down in both directions so the reader still wakes (unread bytes are
/// lost, which is better than a reader asleep for ever), and the error names what failed.
pub(super) fn interrupt(stream: &UnixStream) -> std::io::Result<()> {
    interrupt_with(cancel_pending_read(stream), || stream.shutdown(Shutdown::Both))
}

fn cancel_pending_read(stream: &UnixStream) -> std::io::Result<()> {
    stream.set_nonblocking(true)?;
    // SAFETY: `stream` owns the socket and is borrowed for the whole call, so the handle is valid. A null overlapped
    // pointer cancels every pending operation on the handle.
    let cancelled = unsafe { CancelIoEx(stream.as_raw_socket() as HANDLE, std::ptr::null()) } != 0;
    // SAFETY: `GetLastError` reads this thread's error code.
    cancelled_or(cancelled, || unsafe { GetLastError() })
}

/// What to do with the outcome of the cancel: nothing when it worked, else `fall_back`. The error returned for a failed
/// cancel keeps its kind and says how the fallback went.
fn interrupt_with(cancel: std::io::Result<()>, fall_back: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
    let Err(failure) = cancel else {
        return Ok(());
    };
    let outcome = match fall_back() {
        Ok(()) => "the connection was shut down instead, which discards unread data".to_owned(),
        Err(e) => format!("shutting the connection down instead failed too: {e}"),
    };
    Err(std::io::Error::new(
        failure.kind(),
        format!("cannot cancel the read on the control socket ({failure}); {outcome}"),
    ))
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
