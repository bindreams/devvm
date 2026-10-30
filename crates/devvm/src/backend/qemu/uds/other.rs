use std::net::Shutdown;

use super::UnixStream;

/// Makes every read and write on `stream`'s socket fail or end now, including one blocked in another thread: on Unix
/// `shutdown` wakes a blocked reader. The caller reads the outcome as the peer being gone.
pub(crate) fn interrupt(stream: &UnixStream) {
    // An error here means the socket is closed already, which is what was asked for.
    let _ = stream.shutdown(Shutdown::Both);
}
