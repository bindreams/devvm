//! The AF_UNIX stream type QMP runs over: `uds_windows` on Windows (the standard library has none there), the
//! standard library's elsewhere. The one platform selection of the QMP client, as `host.rs` has for the host.

#[cfg(not(windows))]
use std::os::unix::net as platform;
#[cfg(windows)]
use uds_windows as platform;

#[cfg(test)]
pub(crate) use platform::UnixListener;
pub(crate) use platform::UnixStream;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::interrupt;

#[cfg(not(windows))]
mod other;
#[cfg(not(windows))]
pub(crate) use other::interrupt;
