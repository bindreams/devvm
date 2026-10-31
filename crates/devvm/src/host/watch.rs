//! Waiting for a process while reading its control socket.
//!
//! The reader owns the wait: it waits for the socket and for the process together and nothing is ever cancelled or
//! shut down from another thread, so no byte is discarded and no wakeup can be lost.

use std::io::{Read, Write};
use std::time::Duration;

use crate::Result;
use crate::backend::qemu::uds::UnixStream;

/// How long `ProcessWatch::pause` lasts when the process does not exit.
pub(crate) const RETRY: Duration = Duration::from_millis(10);

/// One process, waited on by whoever reads its control socket. Clones of the `Arc` share it.
pub(crate) trait ProcessWatch: Send + Sync {
    /// Whether the process has exited. Asks the OS each time; an `Err` is a refusal to say, never "no".
    fn has_exited(&self) -> Result<bool>;

    /// Returns `true` as soon as the process has exited, or `false` after `RETRY` if it has not; the caller re-checks
    /// what it is waiting for and calls again. Waits for the process itself, so an exit ends it at once.
    fn pause(&self) -> Result<bool>;

    /// A reader and writer of `stream`, a connection to this process, that stops waiting when the process exits.
    fn link(&self, stream: UnixStream) -> std::io::Result<Box<dyn Link>>;
}

/// A connection to a process's control socket.
///
/// A read returns what the peer wrote, in order, and blocks until there is more. On Windows, once the process has
/// exited and the socket holds nothing more, a read ends the stream (`Ok(0)`) whether or not the OS ever delivers the
/// end of the stream (it can lose it), and a write that cannot finish because the process has exited fails as a closed
/// connection. Elsewhere the link is the bare stream: a read ends when the peer closes it, so a process that keeps the
/// socket open after QEMU exits keeps a read waiting.
pub(crate) trait Link: Read + Write + Send {}

/// A host whose streams always end when the peer's process exits needs nothing but the stream.
impl<T: Read + Write + Send> Link for T {}
