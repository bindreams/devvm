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

pub(super) fn read_link(path: &Path) -> std::io::Result<String> {
    std::fs::read_link(path)?
        .into_os_string()
        .into_string()
        .map_err(|target| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("the symlink target {target:?} is not valid Unicode"),
            )
        })
}

/// Unix symlinks do not care what they point to.
pub(super) fn make_symlink(target: &str, link: &Path, _to_dir: bool) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}
