//! A Unix host, one that devvm 0.1.0 does not run VMs on.

use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rustix::io::Errno;

use super::unsupported::{FileModes, claim_then_rename};

#[cfg(test)]
#[path = "unix_tests.rs"]
mod unix_tests;

pub(super) struct UnixHost;

impl FileModes for UnixHost {
    fn private_options(&self) -> OpenOptions {
        let mut options = OpenOptions::new();
        options.mode(0o600);
        options
    }

    /// `sun_path` less its terminating NUL: 107 on Linux, 103 on macOS and the BSDs.
    fn socket_path_max(&self) -> usize {
        std::mem::size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path) - 1
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

    /// One rename that fails with `AlreadyExists` for anything at `to`, so `from` is there in full or not at all; on
    /// a file system or kernel without such a rename, [`claim_then_rename`].
    fn publish_dir(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        publish_with(from, to, rename_without_replacing)
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

/// Whether `errno` from a rename that refuses to replace says that the file system or the kernel has no such rename,
/// as opposed to refusing this one: NFS answers `EINVAL` to any rename flag, some FUSE and 9p mounts and old kernels
/// `EINVAL` or `ENOSYS`, a seccomp filter `ENOSYS`, macOS volumes without `RENAME_EXCL` `ENOTSUP`.
fn lacks_a_rename_that_refuses_to_replace(errno: Errno) -> bool {
    errno == Errno::INVAL || errno == Errno::NOSYS || errno == Errno::NOTSUP || errno == Errno::OPNOTSUPP
}

/// Publishes with `rename`, which must refuse to replace; where `rename` says there is no such rename, falls back to
/// [`claim_then_rename`].
fn publish_with(from: &Path, to: &Path, rename: impl FnOnce(&Path, &Path) -> Result<(), Errno>) -> std::io::Result<()> {
    match rename(from, to) {
        Err(errno) if lacks_a_rename_that_refuses_to_replace(errno) => claim_then_rename(from, to),
        other => other.map_err(std::io::Error::from),
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
fn rename_without_replacing(from: &Path, to: &Path) -> Result<(), Errno> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE)
}

/// The other Unix systems have no such rename, which is what [`publish_with`] falls back on.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos"
)))]
fn rename_without_replacing(_from: &Path, _to: &Path) -> Result<(), Errno> {
    Err(Errno::NOSYS)
}
