use std::net::Shutdown;
use std::os::windows::io::AsRawSocket;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::IO::CancelIoEx;

use super::UnixStream;

/// Makes every read and write on `stream`'s socket fail now, including one blocked in another thread. Winsock's
/// `shutdown` fails the calls that come after it but does not wake one already blocked (measured), so the pending I/O
/// is cancelled as well. The blocked call returns an error; it is the caller's to read as the peer being gone.
pub(crate) fn interrupt(stream: &UnixStream) {
    // An error here means the socket is closed already, which is what was asked for.
    let _ = stream.shutdown(Shutdown::Both);
    // SAFETY: `stream` owns the socket and is borrowed for the whole call, so the handle is valid. A null overlapped
    // pointer cancels every pending operation on the handle.
    unsafe { CancelIoEx(stream.as_raw_socket() as HANDLE, std::ptr::null()) };
}
