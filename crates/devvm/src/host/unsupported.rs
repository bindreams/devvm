//! What every host devvm 0.1.0 does not run VMs on (Linux, macOS) shares. A host type supplies only what differs, the
//! file modes ([`FileModes`]), and is a [`Host`] through the blanket impl below.

use std::ffi::{OsStr, OsString};
use std::fs::{DirBuilder, Metadata, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cosca::identity::ProcessId;

use super::names::{posix_problem, unix_run_dir};
use super::{
    Accel, Entry, FileId, Hold, HoldOutcome, Host, ProcessWatch, SystemDirs, Unavailable, after_failed_write,
    create_new_with,
};
use crate::exit::ExitWidth;
use crate::{Arch, Result};

mod hold;
mod watch;

/// How a host that cannot run VMs treats file modes and symlinks.
pub(super) trait FileModes: Send + Sync {
    /// Options that create a file readable and writable by its owner only, from the first moment it exists.
    fn private_options(&self) -> OpenOptions;

    /// [`Host::socket_path_max`].
    fn socket_path_max(&self) -> usize;

    /// The effective user id; the runtime directory is per user.
    fn euid(&self) -> u32;

    /// A builder that makes a directory for the owner alone (mode `0700`) from the first moment it exists.
    fn private_dir(&self) -> DirBuilder;

    /// Who owns the object `metadata` describes, if it is not the user running this process.
    fn other_owner(&self, metadata: &Metadata) -> Option<String>;

    /// The identity of the object `metadata` describes, if this host has one.
    fn identity(&self, metadata: &Metadata) -> Option<FileId>;

    fn is_executable(&self, path: &Path) -> std::io::Result<bool>;

    fn set_executable(&self, path: &Path) -> std::io::Result<()>;

    fn read_link(&self, path: &Path) -> std::io::Result<String>;

    fn make_symlink(&self, target: &str, link: &Path, to_dir: bool) -> std::io::Result<()>;

    fn sync_dir(&self, dir: &Path) -> std::io::Result<()>;

    fn open_entry(&self, root: &Path, rel: &str) -> std::io::Result<Entry>;

    /// [`Host::publish_dir`]. A host with a rename that refuses to replace does it in one step; one without can only
    /// call [`claim_then_rename`].
    fn publish_dir(&self, from: &Path, to: &Path) -> std::io::Result<()>;
}

/// Claims `to` with `create_dir`, which fails for anything that exists, and renames `from` over the claim: a rename of
/// a directory replaces an empty one. Of several that race for one name exactly one wins, but a kill between the two
/// steps leaves `to` empty. For a host with no rename that refuses to replace.
pub(super) fn claim_then_rename(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    // The claim is empty, since only this call has it.
    std::fs::rename(from, to).map_err(|e| after_failed_write(to, e, || std::fs::remove_dir(to)))
}

/// A directory at `dir` that is the user's and is no link: `AlreadyExists` for anything else that has the name, and
/// `PermissionDenied` naming the owner for another account's directory.
fn accept_existing(host: &dyn FileModes, dir: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(dir)?;
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} is a link, not a directory", dir.display()),
        ));
    }
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} is not a directory", dir.display()),
        ));
    }
    match host.other_owner(&metadata) {
        None => Ok(()),
        Some(owner) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is owned by {owner}, not by you", dir.display()),
        )),
    }
}

impl<M: FileModes> Host for M {
    fn accelerator(&self, _arch: Arch) -> std::result::Result<Accel, Unavailable> {
        Err(Unavailable {
            missing: format!("{} hosts are not supported in devvm 0.1.0", std::env::consts::OS),
        })
    }

    fn guest_reset(&self, _arch: Arch) -> std::result::Result<(), Unavailable> {
        Ok(())
    }

    fn socket_path_max(&self) -> usize {
        FileModes::socket_path_max(self)
    }

    fn run_dir_default(&self, var: &dyn Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
        Ok(unix_run_dir(var("TMPDIR"), FileModes::euid(self)))
    }

    fn watch_process(&self, id: ProcessId) -> Result<Arc<dyn ProcessWatch>> {
        Ok(Arc::new(watch::UnixWatch::new(id)))
    }

    fn ansi_code_page(&self) -> Option<u32> {
        None
    }

    /// A plain spawn already outlives its parent; the host is refused earlier by `accelerator`.
    fn detach(&self, _cmd: &mut cosca::Command) -> Result<()> {
        Ok(())
    }

    fn exit_width(&self) -> ExitWidth {
        ExitWidth::LowByte
    }

    fn quiet(&self, _cmd: &mut std::process::Command) {}

    fn env_name_strip_prefix(&self, name: &OsStr, prefix: &str) -> Option<String> {
        debug_assert!(prefix.is_ascii());
        name.to_string_lossy().strip_prefix(prefix).map(str::to_owned)
    }

    fn system_dirs(&self) -> Option<SystemDirs> {
        None
    }

    /// Mode `0600` is set when the file is created, so there is no moment with wider permissions.
    fn write_private(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        create_new_with(self.private_options(), path, bytes)
    }

    fn is_executable(&self, path: &Path) -> std::io::Result<bool> {
        FileModes::is_executable(self, path)
    }

    fn set_executable(&self, path: &Path) -> std::io::Result<()> {
        FileModes::set_executable(self, path)
    }

    /// By path through `utimensat`, which asks for no write access to the file.
    fn set_mtime(&self, path: &Path, seconds: u32) -> std::io::Result<()> {
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(i64::from(seconds), 0))
    }

    fn file_name_problem(&self, name: &str) -> Option<String> {
        posix_problem(name)
    }

    fn cannot_hold_path(&self, _rel: &str) -> bool {
        false
    }

    fn replace_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::rename(from, to)
    }

    fn publish_dir(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        FileModes::publish_dir(self, from, to)
    }

    fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        FileModes::sync_dir(self, dir)
    }

    fn read_link(&self, path: &Path) -> std::io::Result<String> {
        FileModes::read_link(self, path)
    }

    fn symlinks_have_kinds(&self) -> bool {
        false
    }

    fn make_symlink(&self, target: &str, link: &Path, to_dir: bool) -> std::io::Result<()> {
        FileModes::make_symlink(self, target, link, to_dir)
    }

    fn open_entry(&self, root: &Path, rel: &str) -> std::io::Result<Entry> {
        FileModes::open_entry(self, root, rel)
    }

    fn create_runtime_dir(&self, dir: &Path) -> std::io::Result<()> {
        match self.private_dir().create(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => accept_existing(self, dir),
            other => other,
        }
    }

    fn file_id(&self, path: &Path) -> std::io::Result<FileId> {
        let metadata = std::fs::symlink_metadata(path)?;
        self.identity(&metadata)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Unsupported, "this build cannot tell files apart"))
    }

    fn hold_new(&self, path: &Path) -> std::io::Result<Box<dyn Hold>> {
        hold::new(self.private_options(), path, &|metadata| self.identity(metadata))
    }

    fn hold_existing(&self, path: &Path) -> std::io::Result<HoldOutcome> {
        hold::existing(path, &|metadata| self.identity(metadata))
    }
}
