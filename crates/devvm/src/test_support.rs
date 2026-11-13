//! Helpers shared by the unit tests (compiled only for `cfg(test)`).

pub mod alloc;
pub mod callers;
pub mod claimed;
pub mod fake_vm;
pub mod file_mode;
#[cfg(windows)]
pub mod file_security;
pub mod fixtures;
pub mod helper;
pub mod host;
pub mod http;
pub mod manual_exit;
pub mod name_change;
pub mod other_account;
pub mod qemu;
pub mod refusing;
pub mod roots;
pub mod sshd;
pub mod stub_backend;
pub mod symlink;
pub mod transport;
pub mod unix_files;
pub mod wrapped_programs;

// `roots.rs` names `super::UNICODE_DIR`, as it does in the test binary that includes it.
use fixtures::UNICODE_DIR;
