//! The limit on a control socket path of a Windows host, as `WindowsHost::socket_path_max` gives it.

/// `sun_path` is 108 bytes, less its terminating NUL.
pub const SOCKET_PATH_MAX: usize = 107;
