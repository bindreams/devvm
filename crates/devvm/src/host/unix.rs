//! A Unix host, one that devvm 0.1.0 does not run VMs on.

use std::fs::{DirBuilder, File, FileType, Metadata, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, UTIME_OMIT, utimensat};
use rustix::io::Errno;

use super::entry;
use super::unsupported::{FileModes, claim_then_rename};
use super::{Entry, FileId, MakePrivateError, MakePrivateOutcome, refused_as_link};

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

    /// Opens `path` once, without following a link and without waiting on a pipe ([`open_regular`]), and looks at and
    /// changes that descriptor. A mode with no group or other permission bits is left as it is; any other is set to
    /// `0600`. A name that is not a regular file is refused, and so is a file its owner cannot read.
    fn make_private(&self, path: &Path) -> Result<MakePrivateOutcome, MakePrivateError> {
        let file = open_regular(path).map_err(|e| MakePrivateError::Check(refused_as_link(path, e)))?;
        let metadata = file.metadata().map_err(MakePrivateError::Check)?;
        if !metadata.is_file() {
            return Err(MakePrivateError::Check(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            )));
        }
        if metadata.permissions().mode() & 0o077 == 0 {
            return Ok(MakePrivateOutcome::Unchanged);
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(MakePrivateError::Change)?;
        Ok(MakePrivateOutcome::Changed)
    }

    fn hold_options(&self) -> OpenOptions {
        let mut options = OpenOptions::new();
        options.read(true).write(true).custom_flags(libc::O_NOFOLLOW);
        options
    }

    /// `sun_path` less its terminating NUL: 107 on Linux, 103 on macOS and the BSDs.
    fn socket_path_max(&self) -> usize {
        std::mem::size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path) - 1
    }

    fn euid(&self) -> u32 {
        // SAFETY: `geteuid` reads a process attribute and cannot fail.
        unsafe { libc::geteuid() }
    }

    fn private_dir(&self) -> DirBuilder {
        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        builder
    }

    fn other_owner(&self, metadata: &Metadata) -> Option<String> {
        (metadata.uid() != self.euid()).then(|| format!("user {}", metadata.uid()))
    }

    fn identity(&self, metadata: &Metadata) -> Option<FileId> {
        Some(FileId {
            volume: metadata.dev(),
            file: u128::from(metadata.ino()),
        })
    }

    fn lacks_hard_links(&self, error: &std::io::Error) -> bool {
        lacks_hard_links(error)
    }

    /// `f_bavail` blocks of `f_frsize` bytes: what is free to an unprivileged user.
    fn free_bytes(&self, existing: &Path) -> std::io::Result<u64> {
        let stat = rustix::fs::statvfs(existing).map_err(std::io::Error::from)?;
        Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
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

    fn open_entry(&self, root: &Path, rel: &str) -> std::io::Result<Entry> {
        entry::open_entry(root, rel, open_regular, describe)
    }

    /// Unix symlinks do not care what they point to.
    fn make_symlink(&self, target: &str, link: &Path, _to_dir: bool) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }
}

/// Sets the modification time of `path` (through a link) and leaves the access time alone. It needs ownership only and
/// opens nothing, so a file or directory its owner cannot read takes it, and a FIFO does not block it.
pub(crate) fn set_modified(path: &Path, time: SystemTime) -> std::io::Result<()> {
    let modified = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => Timespec::try_from(after),
        Err(before) => Timespec::try_from(before.duration()).map(|ago| -ago),
    }
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "time out of range"))?;
    let times = Timestamps {
        last_access: Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        },
        last_modification: modified,
    };
    utimensat(CWD, path, &times, AtFlags::empty()).map_err(std::io::Error::from)
}

/// Opens `path` for reading, but not a link and not a pipe's other end: a link that appeared since the file was looked
/// at fails the open, and a FIFO opens without waiting for a writer.
fn open_regular(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

fn describe(kind: FileType) -> &'static str {
    if kind.is_fifo() {
        "a FIFO"
    } else if kind.is_socket() {
        "a socket"
    } else if kind.is_char_device() {
        "a character device"
    } else if kind.is_block_device() {
        "a block device"
    } else {
        "of a type this host does not know"
    }
}

/// Whether `error` from `link(2)` says that the file system has no hard links: `EPERM` on Linux, `ENOTSUP` or
/// `EOPNOTSUPP` elsewhere. `EACCES` is no such statement: a directory that cannot be written, or a security module,
/// answers it for any file system.
fn lacks_hard_links(error: &std::io::Error) -> bool {
    Errno::from_io_error(error)
        .is_some_and(|errno| errno == Errno::PERM || errno == Errno::NOTSUP || errno == Errno::OPNOTSUPP)
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
    use rustix::fs::{RenameFlags, renameat_with};
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
