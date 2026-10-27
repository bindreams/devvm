//! File permissions on a Unix host.

use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Options that create a file readable and writable by its owner only, from the first moment it exists.
pub(super) fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.mode(0o600);
    options
}

pub(super) fn is_executable(path: &Path) -> std::io::Result<bool> {
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o111 != 0)
}

/// Adds an execute bit for each of owner, group and others that can read the file.
pub(super) fn set_executable(path: &Path) -> std::io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    let mode = permissions.mode();
    permissions.set_mode(mode | ((mode & 0o444) >> 2));
    std::fs::set_permissions(path, permissions)
}
