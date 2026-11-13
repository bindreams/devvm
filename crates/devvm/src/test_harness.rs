//! Labels the lib's tests declare for skuld to select by (`SKULD_LABELS=<label>`).

/// Selects the `VM` group's tests: every test that boots QEMU or runs one of its tools.
#[skuld::label]
pub const VM: skuld::Label;

/// Selects the tests that boot a Linux guest, and is what `vm_engine` is serial with: they run one at a time, because
/// several 2 vCPU, 2 GiB guests at once on a 4 vCPU runner can pass the boot bound under load.
#[skuld::label]
pub const GUEST: skuld::Label;

/// Selects the tests of the SFTP client (`access/sftp_tests.rs`), which also run against OpenSSH's `sftp-server`.
#[skuld::label]
pub const SFTP: skuld::Label;

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

#[skuld::label]
pub const UNICODE_BASE: skuld::Label;

#[skuld::label]
pub const SYMLINKS: skuld::Label;

#[skuld::label]
pub const EXFAT_VOLUME: skuld::Label;

/// Selects the tests that build and boot a real Windows guest from `DEVVM_WINDOWS_ISO`.
#[skuld::label]
pub const WINDOWS_GUEST: skuld::Label;

/// Selects the tests that run the Windows guest's wrapped commands in a real `pwsh`.
#[skuld::label]
pub const PWSH_WRAP: skuld::Label;
