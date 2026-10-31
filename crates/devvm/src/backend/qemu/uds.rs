//! The AF_UNIX stream type QMP runs over: `uds_windows` on Windows (the standard library has none there), the
//! standard library's elsewhere. What differs beyond the type is behind `Host`.

#[cfg(not(windows))]
use std::os::unix::net as platform;
#[cfg(windows)]
use uds_windows as platform;

#[cfg(test)]
pub(crate) use platform::UnixListener;
pub(crate) use platform::UnixStream;
