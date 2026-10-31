//! A Unix host, one that devvm 0.1.0 does not run VMs on.

use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use super::unsupported::FileModes;

pub(super) struct UnixHost;

impl FileModes for UnixHost {
    fn private_options(&self) -> OpenOptions {
        let mut options = OpenOptions::new();
        options.mode(0o600);
        options
    }

    fn is_executable(&self, path: &Path) -> std::io::Result<bool> {
        Ok(std::fs::metadata(path)?.permissions().mode() & 0o111 != 0)
    }

    /// Adds an execute bit for each of owner, group and others that can read the file.
    fn set_executable(&self, path: &Path) -> std::io::Result<()> {
        let mut permissions = std::fs::metadata(path)?.permissions();
        let mode = permissions.mode();
        permissions.set_mode(mode | ((mode & 0o444) >> 2));
        std::fs::set_permissions(path, permissions)
    }

    fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::File::open(dir)?.sync_all()
    }

    fn read_link(&self, path: &Path) -> std::io::Result<String> {
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
    fn make_symlink(&self, target: &str, link: &Path, _to_dir: bool) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }
}
