//! What differs between host operating systems, behind one trait. The implementation is chosen here and only here, by
//! the `cfg`s on the module declarations and on [`Current`] below; no code under `host/` selects one.

use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::exit::ExitWidth;
use crate::{Arch, Result};

#[cfg(test)]
#[path = "host_tests.rs"]
mod host_tests;

pub(crate) mod names;
#[cfg_attr(
    any(unix, windows),
    allow(
        dead_code,
        reason = "the host on neither Unix nor Windows; the tests use it everywhere"
    )
)]
mod other;
#[cfg(unix)]
mod unix;
mod unsupported;
#[cfg(windows)]
mod windows;

#[cfg(not(any(unix, windows)))]
use other::OtherHost as Current;
#[cfg(unix)]
use unix::UnixHost as Current;
#[cfg(windows)]
use windows::WindowsHost as Current;

/// How a hypervisor is reached on this host.
#[allow(dead_code, reason = "unused until the QEMU backend lands")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Accel {
    Whpx,
    Kvm,
    Hvf,
    /// QEMU's software emulation; works everywhere, slowly.
    Tcg,
}

/// The host cannot run VMs; `missing` names the piece that is absent.
#[derive(Debug)]
pub struct Unavailable {
    pub missing: String,
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.missing)
    }
}

impl std::error::Error for Unavailable {}

/// The Windows directories, as the OS reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SystemDirs {
    pub root: PathBuf,
    pub system32: PathBuf,
}

pub(crate) trait Host: Send + Sync {
    /// The accelerator for guests of `arch`, or what is missing.
    fn accelerator(&self, arch: Arch) -> std::result::Result<Accel, Unavailable>;

    /// The ANSI code page of this process (`GetACP`); `None` off Windows.
    fn ansi_code_page(&self) -> Option<u32>;

    /// Starts `cmd` without a console, so no console control event reaches the VM. It never breaks the process away
    /// from an enclosing job: whatever the outer process installed is not devvm's business.
    fn detach(&self, cmd: &mut cosca::Command) -> Result<()>;

    /// How many bits of a guest exit code this host's processes can pass on.
    fn exit_width(&self) -> ExitWidth;

    /// Prepares a short non-interactive tool (`qemu-img`, `git`, `ssh -G`) to run without a console window.
    fn quiet(&self, cmd: &mut std::process::Command);

    /// The Windows and System32 directories from the OS (never from the environment); `None` off Windows.
    fn system_dirs(&self) -> Option<SystemDirs>;

    /// Creates `path` with `bytes`, readable by the current user only from the moment it exists; fails with
    /// `AlreadyExists` instead of replacing a file. Windows has no mode bits: the file keeps the ACL it inherits.
    fn write_private(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()>;

    /// Whether any execute bit of `path` is set; always `false` on Windows.
    fn is_executable(&self, path: &Path) -> std::io::Result<bool>;

    /// Adds execute where read is set; does nothing on Windows.
    fn set_executable(&self, path: &Path) -> std::io::Result<()>;

    /// Sets the modification time of the file `path` to `seconds` since 1970. It needs no write access to the file's
    /// contents, so a file that is read-only to its owner takes it.
    fn set_file_mtime(&self, path: &Path, seconds: u32) -> std::io::Result<()>;

    /// Why `name`, a file name taken from a guest, cannot be one file name in a directory on this host (a Windows
    /// host refuses `a\b`, `C:\z`, device names and more); `None` when it can.
    fn file_name_problem(&self, name: &str) -> Option<String>;

    /// `name` the way this host's file system compares names: two names with the same key are one file here, so
    /// writing both overwrites the first.
    fn name_key(&self, name: &str) -> String;

    /// Moves the file or symlink `from` over `to`, replacing what is there even when it is read-only (Windows refuses
    /// to rename over a read-only file). A directory at `to` is an error.
    fn replace_file(&self, from: &Path, to: &Path) -> std::io::Result<()>;

    /// Moves the directory `from` to `to`, which nobody has: an existing `to`, even an empty directory, is
    /// `AlreadyExists` and nothing changes. Unlike a rename, it is a publish: of several that race for one name,
    /// exactly one wins. Where the file system has a rename that refuses to replace (Windows, Linux, macOS), `to` is
    /// there in full or not at all; where it has none (NFS, some FUSE mounts, old kernels) `to` is claimed first, and
    /// a kill between the claim and the move leaves it empty.
    fn publish_dir(&self, from: &Path, to: &Path) -> std::io::Result<()>;

    /// Flushes the entries of `dir` to disk, so a file or directory made in it survives a crash. Does nothing on
    /// Windows, where the file system journals them.
    fn sync_dir(&self, dir: &Path) -> std::io::Result<()>;

    /// What the symlink at `path` points to, spelled with `/` as SFTP and Unix guests do; fails with `InvalidData` when
    /// the target is not valid Unicode.
    fn read_link(&self, path: &Path) -> std::io::Result<String>;

    /// Creates the symlink `link` pointing to `target`, a `/`-separated text taken from a guest. `to_dir` says whether
    /// the target is a directory, which Windows needs to know. Fails with `PermissionDenied`, naming the cause, where
    /// this host cannot make symlinks (Windows without Developer Mode or an elevated token).
    fn make_symlink(&self, target: &str, link: &Path, to_dir: bool) -> std::io::Result<()>;
}

/// Creates `path` with `options` and writes `bytes`, which are on disk when it returns. A file whose write failed is
/// removed, so a half-written secret is never left behind.
fn create_new_with(mut options: OpenOptions, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = options.write(true).create_new(true).open(path)?;
    if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        return Err(after_failed_write(path, e, || std::fs::remove_file(path)));
    }
    Ok(())
}

/// Removes the file `path`; one that is not there is as good as removed.
pub(crate) fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// The error of a write to `path` that failed with `error`, after `remove` took what the write left away. A leftover
/// that stays is named in the error: a retry would otherwise meet `AlreadyExists` with no explanation.
fn after_failed_write(
    path: &Path,
    error: std::io::Error,
    remove: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Error {
    match remove() {
        Ok(()) => error,
        Err(left) => std::io::Error::new(
            error.kind(),
            format!(
                "{error}; the leftover {} could not be removed either ({left})",
                path.display()
            ),
        ),
    }
}

/// The host this binary runs on.
pub(crate) fn current() -> &'static dyn Host {
    static CURRENT: Current = Current;
    &CURRENT
}

/// How many bits of a guest exit code the host can pass on.
pub fn host_exit_width() -> ExitWidth {
    current().exit_width()
}
