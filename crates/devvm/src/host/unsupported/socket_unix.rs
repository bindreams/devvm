//! The AF_UNIX path limit of a Unix host.

/// `sun_path` less its terminating NUL: 107 on Linux, 103 on macOS and the BSDs.
pub(super) fn path_max() -> usize {
    std::mem::size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path) - 1
}
