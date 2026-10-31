//! Files that only Unix can have, for tests that fill a guest directory served by a real SFTP server. Off Unix the
//! tests that need them do not run, and these fail.

#[cfg(unix)]
#[path = "unix_files/unix.rs"]
mod imp;
#[cfg(not(unix))]
#[path = "unix_files/other.rs"]
mod imp;

pub use imp::{make_fifo, make_file_with_name_bytes, make_symlink_with_target_bytes};
