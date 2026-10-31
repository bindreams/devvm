//! Helpers shared by the unit tests (compiled only for `cfg(test)`).

pub mod alloc;
pub mod callers;
pub mod fake_vm;
pub mod file_mode;
pub mod fixtures;
pub mod helper;
pub mod host;
pub mod http;
pub mod manual_exit;
pub mod qemu;
pub mod refusing;
pub mod sshd;
pub mod stub_backend;
pub mod symlink;
pub mod unix_files;
