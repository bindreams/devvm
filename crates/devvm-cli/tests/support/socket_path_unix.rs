//! The limit on a control socket path of a Unix host, computed as `UnixHost::socket_path_max` computes it.

/// `sun_path` less its terminating NUL.
pub const SOCKET_PATH_MAX: usize =
    std::mem::size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path) - 1;
