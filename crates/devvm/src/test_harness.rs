//! Labels the lib's tests declare for skuld to select by (`SKULD_LABELS=<label>`).

/// Selects the `VM` group's tests: every test that boots QEMU or runs one of its tools.
#[skuld::label]
pub const VM: skuld::Label;

#[skuld::label]
pub const PYYAML: skuld::Label;

#[skuld::label]
pub const OPENSSH_SFTP: skuld::Label;

#[skuld::label]
pub const NTFS_NAMES: skuld::Label;

#[skuld::label]
pub const ACP_1252: skuld::Label;

#[skuld::label]
pub const UNICODE_SOCKET_DIR: skuld::Label;
