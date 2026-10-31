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

    /// One rename that fails with `AlreadyExists` for anything at `to`, so `from` is there in full or not at all.
    fn publish_dir(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        rename_without_replacing(from, to)
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

/// `renameat2(RENAME_NOREPLACE)` on Linux, `renamex_np(RENAME_EXCL)` on macOS.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos"
))]
fn rename_without_replacing(from: &Path, to: &Path) -> std::io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(std::io::Error::from)
}

/// The other Unix systems have no such rename.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos"
)))]
fn rename_without_replacing(from: &Path, to: &Path) -> std::io::Result<()> {
    super::unsupported::claim_then_rename(from, to)
}
