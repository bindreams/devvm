//! Labels the lib's tests declare for skuld to select by (`SKULD_LABELS=<label>`).

/// Selects the `VM_GROUP` group's tests: every test that boots QEMU or runs one of its tools.
#[skuld::label]
pub const VM: skuld::Label;

/// Selects the `PYYAML` group's test.
#[skuld::label]
pub const PYYAML: skuld::Label;

/// Selects the `OPENSSH_SFTP` group's test.
#[skuld::label]
pub const OPENSSH_SFTP: skuld::Label;

/// Selects the `NTFS_NAMES` group's test.
#[skuld::label]
pub const NTFS_NAMES: skuld::Label;

/// Selects the `ACP_1252` group's test.
#[skuld::label]
pub const ACP_1252: skuld::Label;

/// Selects the `UNICODE_SOCKET_DIR` group's tests.
#[skuld::label]
pub const UNICODE_SOCKET_DIR: skuld::Label;
