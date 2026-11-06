//! What differs between host operating systems, behind one trait. The implementation is chosen here and only here, by
//! the `cfg`s on the module declarations and on [`Current`] below; no code under `host/` selects one.
//! (`backend/qemu/uds.rs` only names the AF_UNIX stream type each OS has.)

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cosca::identity::ProcessId;

use crate::exit::ExitWidth;
use crate::{Arch, Result};

#[cfg(test)]
#[path = "host_tests.rs"]
mod host_tests;

mod entry;
mod file_id;
mod hold;
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
pub(crate) mod watch;
#[cfg(windows)]
pub(crate) mod windows;

pub(crate) use entry::{Entry, is_a_link, is_absent, look_failed, symlink_metadata_if_present};
pub(crate) use file_id::{Comparison, FileId};
#[cfg_attr(
    not(windows),
    allow(
        unused_imports,
        reason = "only the Windows host renames a claim file away; the tests use it everywhere"
    )
)]
pub(crate) use hold::{DELETING_DRAW_BYTES, deleting_name};
pub(crate) use hold::{Hold, HoldOutcome, claim_of_deleting_name, claim_of_new_name, new_name};
pub(crate) use watch::{Link, ProcessWatch};

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

    /// Whether a running guest of `arch` can be reset inside its VM process (a QMP `system_reset`). The error says what
    /// the host lacks; nothing is sent to a VM to find out.
    fn guest_reset(&self, arch: Arch) -> std::result::Result<(), Unavailable>;

    /// The longest AF_UNIX socket path this OS binds, in bytes: `sun_path` without its terminating NUL.
    fn socket_path_max(&self) -> usize;

    /// Where this user's runtime state (the QMP sockets) goes when `XDG_RUNTIME_DIR` is not usable, resolved from the
    /// OS and `var` (the environment) and created by nobody. Windows: `<ProgramData>\devvm-run-<16 hex of SHA-256 of
    /// the user's SID>`, from the known folder and the process token, never from the environment. Elsewhere:
    /// `<TMPDIR or /tmp>/devvm-run-<euid>`. An error names the OS call that failed.
    fn run_dir_default(&self, var: &dyn Fn(&str) -> Option<OsString>) -> Result<PathBuf>;

    /// Watches the process `id` names. On Windows its exit ends a read of that process's control socket through the
    /// watch's `link` even when the OS never ends the stream (Windows can lose the end of an AF_UNIX stream); elsewhere
    /// the link is the bare stream and a read ends when the peer closes it, and the exit is seen by `has_exited` and
    /// `pause`. A process that has exited already is a watch that reports it. An error means the OS would not let the
    /// process be watched.
    fn watch_process(&self, id: ProcessId) -> Result<Arc<dyn ProcessWatch>>;

    /// The ANSI code page of this process (`GetACP`); `None` off Windows.
    fn ansi_code_page(&self) -> Option<u32>;

    /// Starts `cmd` without a console, so no console control event reaches the VM. It never breaks the process away
    /// from an enclosing job: whatever the outer process installed is not devvm's business.
    fn detach(&self, cmd: &mut cosca::Command) -> Result<()>;

    /// How many bits of a guest exit code this host's processes can pass on.
    fn exit_width(&self) -> ExitWidth;

    /// Prepares a short non-interactive tool (`qemu-img`, `git`, `ssh -G`) to run without a console window.
    fn quiet(&self, cmd: &mut std::process::Command);

    /// What follows `prefix` in the environment variable `name`, if `name` starts with it as this host compares
    /// names; the rest is lossy UTF-8. `prefix` is ASCII. Windows compares with `CompareStringOrdinal` (ignore case, OS
    /// uppercase table: U+017F is no `S`); elsewhere bytes must be equal.
    fn env_name_strip_prefix(&self, name: &OsStr, prefix: &str) -> Option<String>;

    /// The Windows and System32 directories from the OS (never from the environment); `None` off Windows.
    fn system_dirs(&self) -> Option<SystemDirs>;

    /// Creates `path` with `bytes`, readable by the current user only from the moment it exists; fails with
    /// `AlreadyExists` instead of replacing a file. Windows has no mode bits: the file keeps the ACL it inherits.
    fn write_private(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()>;

    /// Whether any execute bit of `path` is set; always `false` on Windows.
    fn is_executable(&self, path: &Path) -> std::io::Result<bool>;

    /// Adds execute where read is set; does nothing on Windows.
    fn set_executable(&self, path: &Path) -> std::io::Result<()>;

    /// Sets the modification time of the file or directory `path` to `seconds` since 1970. It needs no write access to
    /// the contents, so a file that is read-only to its owner takes it.
    fn set_mtime(&self, path: &Path, seconds: u32) -> std::io::Result<()>;

    /// Why `name`, a file name taken from a guest, cannot be one file name in a directory on this host (a Windows
    /// host refuses `a\b`, `C:\z`, device names and more); `None` when it can.
    fn file_name_problem(&self, name: &str) -> Option<String>;

    /// Whether this host reads some segment of `rel` (`/`-separated, a path git lists) as other than a file name, so
    /// the path is no file git tracks here (a Windows host reads `name:stream` and `d:x` that way). Decided from the
    /// text alone, before the file system is asked anything.
    fn cannot_hold_path(&self, rel: &str) -> bool;

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

    /// Whether this host makes a symlink to a directory and a symlink to a file differently (Windows does), so
    /// [`Host::make_symlink`] needs to be told which the target is. A copy looks at the target of a guest link only
    /// when this is `true`.
    fn symlinks_have_kinds(&self) -> bool;

    /// Creates the symlink `link` pointing to `target`, a `/`-separated text taken from a guest. `to_dir` says whether
    /// the target is a directory, which a host with [`Host::symlinks_have_kinds`] needs to know; any other host
    /// ignores it. Fails with `PermissionDenied`, naming the cause, where
    /// this host cannot make symlinks (Windows without Developer Mode or an elevated token).
    fn make_symlink(&self, target: &str, link: &Path, to_dir: bool) -> std::io::Result<()>;

    /// Makes `dir`, one level, for this user alone from the moment it exists (Unix: mode `0700`; Windows: owner the
    /// user and a protected DACL with the user's one entry), or accepts a directory that exists and is the user's.
    /// Anything else at `dir` is refused: a file, a link or junction, and a directory of another account
    /// (`PermissionDenied`, `<dir> is owned by <account>, not by you`). The owner is the whole check: a directory the
    /// user owns with a looser mode or DACL is accepted, since that is the user's own setting. A missing parent is
    /// `NotFound`.
    fn create_runtime_dir(&self, dir: &Path) -> std::io::Result<()>;

    /// The check [`Host::create_runtime_dir`] applies to a directory that exists, with nothing made: `Ok` for a
    /// directory that is the user's and is no link, and the same refusals for anything else. A directory that is not
    /// there is `NotFound`.
    fn accept_runtime_dir(&self, dir: &Path) -> std::io::Result<()>;

    /// The identity of the file `path` names, not of what a link there points to. It needs no access to the contents,
    /// so it reads the identity of a file that is held (see [`Hold`]).
    fn file_id(&self, path: &Path) -> std::io::Result<FileId>;

    /// The identity of the open file `file`, whatever its name leads to now. While `file` stays open
    /// the id cannot be given to another file (ext4 reuses an inode only after the last close). On Windows it fails
    /// with `InvalidInput` on a volume without file ids (exFAT, FAT32); a build without file ids fails `Unsupported`.
    fn file_id_of_open(&self, file: &File) -> std::io::Result<FileId>;

    /// Whether the volume of the open file `file` has POSIX unlink and rename, which devvm's lock files rely on (NTFS
    /// and ReFS have it; exFAT, FAT32 and SMB do not). Hosts other than Windows have it everywhere.
    fn volume_has_posix_unlink_rename(&self, file: &File) -> std::io::Result<bool>;

    /// Creates `path`, which must not exist (`AlreadyExists`), private to the user, and holds it. The name never
    /// exists without the hold: off Windows the file is made and locked as `<path>.new` and then linked to `path`
    /// (a launch killed in between leaves the `.new` file; a file there is `AlreadyExists` too). A file system that
    /// does not allow hard links is an error that names the directory and `XDG_RUNTIME_DIR`.
    fn hold_new(&self, path: &Path) -> std::io::Result<Box<dyn Hold>>;

    /// Opens the lock file `path` for reading and writing with the default sharing (other opens, deletes and renames
    /// are allowed), making it if `create`. A link at `path` is `InvalidInput`, "is a link": it is not opened through,
    /// so a lock is never taken on what a link leads to. A host whose open cannot refuse a link (`OtherHost`) looks at
    /// the name first, so a link made between the look and the open is followed there.
    fn open_lock_file(&self, path: &Path, create: bool) -> std::io::Result<File>;

    /// Holds the existing file `path` if nobody else does. It never waits and never creates the file, and a link at
    /// `path` is `InvalidInput`, "is a link": it is not opened through.
    fn hold_existing(&self, path: &Path) -> std::io::Result<HoldOutcome>;

    /// Removes `path`, a leftover claim file under its deleting name ([`deleting_name`]): its name may stay until the
    /// last handle to the file closes. `Ok` also when someone is deleting it right now (Windows: it is held, or its
    /// deletion is pending), which is theirs to finish. Any other failure, `NotFound` included, is returned.
    fn remove_deleting_name(&self, path: &Path) -> std::io::Result<()>;

    /// Looks at `rel` (`/`-separated, a path git lists) below the directory `root`, by name, without following a link
    /// at the last name: its kind comes from `symlink_metadata`, and a regular file is opened for reading and
    /// confirmed to be one from the open handle. A path that is gone, or is below a file, or has a name the host
    /// cannot hold, is [`Entry::Absent`]. A link above the last name is not looked for; the caller checks the
    /// directories. Fails with `Unsupported` where a host has no such look.
    fn open_entry(&self, root: &Path, rel: &str) -> std::io::Result<Entry>;
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

/// Deletes whatever is at `path`, a name devvm owns, and succeeds when nothing is there: a link is deleted as a link and
/// never followed, a directory goes with everything under it, anything else is removed as a file.
///
/// Callers exclude other deleters and creators of the name.
pub(crate) fn remove_tree(path: &Path) -> std::io::Result<()> {
    match symlink_metadata_if_present(path).map_err(|e| look_failed(path, e))? {
        None => Ok(()),
        Some(meta) if meta.file_type().is_symlink() => remove_symlink(path),
        Some(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Some(_) => std::fs::remove_file(path),
    }
}

/// Removes the symlink `path`. A symlink to a directory is removed as a directory on Windows and as a file elsewhere;
/// a junction is removed as a directory.
pub(crate) fn remove_symlink(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
        .or_else(|as_file| std::fs::remove_dir(path).map_err(|as_dir| link_removal_failure(as_file, as_dir)))
}

/// The error of a symlink that could be removed neither as a file nor as a directory. The first failure is the cause on
/// every host but Windows, where it can be the second; both are said.
fn link_removal_failure(as_file: std::io::Error, as_dir: std::io::Error) -> std::io::Error {
    if as_file.to_string() == as_dir.to_string() {
        return as_file;
    }
    let text = format!("{as_file} (removing it as a directory link failed too: {as_dir})");
    std::io::Error::new(as_file.kind(), text)
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
