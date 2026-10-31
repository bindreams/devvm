//! Hosts for the tests: one whose accelerator always works, and one that has none.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cosca::identity::ProcessId;

use crate::exit::ExitWidth;
use crate::host::{self, Accel, Entry, Host, ProcessWatch, SystemDirs, Unavailable, names};
use crate::{Arch, Result};

/// Reports TCG as the accelerator and delegates everything else to the real host, so the real `detach` is the one the
/// tests exercise.
pub struct TestHost;

impl Host for TestHost {
    fn accelerator(&self, _arch: Arch) -> std::result::Result<Accel, Unavailable> {
        Ok(Accel::Tcg)
    }

    fn guest_reset(&self, arch: Arch) -> std::result::Result<(), Unavailable> {
        host::current().guest_reset(arch)
    }

    fn socket_path_max(&self) -> usize {
        host::current().socket_path_max()
    }

    fn watch_process(&self, id: ProcessId) -> Result<Arc<dyn ProcessWatch>> {
        host::current().watch_process(id)
    }

    fn ansi_code_page(&self) -> Option<u32> {
        host::current().ansi_code_page()
    }

    fn detach(&self, cmd: &mut cosca::Command) -> Result<()> {
        host::current().detach(cmd)
    }

    fn exit_width(&self) -> ExitWidth {
        host::current().exit_width()
    }

    fn quiet(&self, cmd: &mut std::process::Command) {
        host::current().quiet(cmd)
    }

    fn env_name_strip_prefix(&self, name: &OsStr, prefix: &str) -> Option<String> {
        host::current().env_name_strip_prefix(name, prefix)
    }

    fn system_dirs(&self) -> Option<SystemDirs> {
        host::current().system_dirs()
    }

    fn write_private(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        host::current().write_private(path, bytes)
    }

    fn is_executable(&self, path: &Path) -> std::io::Result<bool> {
        host::current().is_executable(path)
    }

    fn set_executable(&self, path: &Path) -> std::io::Result<()> {
        host::current().set_executable(path)
    }

    fn set_mtime(&self, path: &Path, seconds: u32) -> std::io::Result<()> {
        host::current().set_mtime(path, seconds)
    }

    fn file_name_problem(&self, name: &str) -> Option<String> {
        host::current().file_name_problem(name)
    }

    fn replace_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        host::current().replace_file(from, to)
    }

    fn publish_dir(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        host::current().publish_dir(from, to)
    }

    fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        host::current().sync_dir(dir)
    }

    fn read_link(&self, path: &Path) -> std::io::Result<String> {
        host::current().read_link(path)
    }

    fn symlinks_have_kinds(&self) -> bool {
        host::current().symlinks_have_kinds()
    }

    fn make_symlink(&self, target: &str, link: &Path, to_dir: bool) -> std::io::Result<()> {
        host::current().make_symlink(target, link, to_dir)
    }

    fn open_entry(&self, root: &Path, rel: &str) -> std::io::Result<Entry> {
        host::current().open_entry(root, rel)
    }
}

/// What [`NoAccelHost`] says is missing.
pub const NO_ACCELERATOR: &str = "the test host has no hypervisor";

/// A host without a hypervisor, otherwise like [`TestHost`].
pub struct NoAccelHost;

impl Host for NoAccelHost {
    fn accelerator(&self, _arch: Arch) -> std::result::Result<Accel, Unavailable> {
        Err(Unavailable {
            missing: NO_ACCELERATOR.to_owned(),
        })
    }

    fn guest_reset(&self, arch: Arch) -> std::result::Result<(), Unavailable> {
        TestHost.guest_reset(arch)
    }

    fn socket_path_max(&self) -> usize {
        TestHost.socket_path_max()
    }

    fn watch_process(&self, id: ProcessId) -> Result<Arc<dyn ProcessWatch>> {
        TestHost.watch_process(id)
    }

    fn ansi_code_page(&self) -> Option<u32> {
        TestHost.ansi_code_page()
    }

    fn detach(&self, cmd: &mut cosca::Command) -> Result<()> {
        TestHost.detach(cmd)
    }

    fn exit_width(&self) -> ExitWidth {
        TestHost.exit_width()
    }

    fn quiet(&self, cmd: &mut std::process::Command) {
        TestHost.quiet(cmd)
    }

    fn env_name_strip_prefix(&self, name: &OsStr, prefix: &str) -> Option<String> {
        TestHost.env_name_strip_prefix(name, prefix)
    }

    fn system_dirs(&self) -> Option<SystemDirs> {
        TestHost.system_dirs()
    }

    fn write_private(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        TestHost.write_private(path, bytes)
    }

    fn is_executable(&self, path: &Path) -> std::io::Result<bool> {
        TestHost.is_executable(path)
    }

    fn set_executable(&self, path: &Path) -> std::io::Result<()> {
        TestHost.set_executable(path)
    }

    fn set_mtime(&self, path: &Path, seconds: u32) -> std::io::Result<()> {
        TestHost.set_mtime(path, seconds)
    }

    fn file_name_problem(&self, name: &str) -> Option<String> {
        TestHost.file_name_problem(name)
    }

    fn replace_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        TestHost.replace_file(from, to)
    }

    fn publish_dir(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        TestHost.publish_dir(from, to)
    }

    fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        TestHost.sync_dir(dir)
    }

    fn read_link(&self, path: &Path) -> std::io::Result<String> {
        TestHost.read_link(path)
    }

    fn symlinks_have_kinds(&self) -> bool {
        TestHost.symlinks_have_kinds()
    }

    fn make_symlink(&self, target: &str, link: &Path, to_dir: bool) -> std::io::Result<()> {
        TestHost.make_symlink(target, link, to_dir)
    }

    fn open_entry(&self, root: &Path, rel: &str) -> std::io::Result<Entry> {
        TestHost.open_entry(root, rel)
    }
}

/// [`TestHost`] that records the file-permission calls and can be told to fail a private write, to report files as
/// executable or to apply a Windows host's file-name rules, so code that goes through [`Host`] can be tested on every
/// OS.
#[derive(Default)]
pub struct RecordingHost {
    windows_names: bool,
    no_symlinks: bool,
    /// What `symlinks_have_kinds` answers; the real host's answer when unset.
    link_kinds: Option<bool>,
    /// The `to_dir` of every `make_symlink` call, in call order, failed calls included.
    symlinks_made: Mutex<Vec<bool>>,
    private_writes: Mutex<Vec<PathBuf>>,
    fail_private_write_at: Mutex<Option<usize>>,
    executable: Mutex<HashSet<PathBuf>>,
    set_executable_calls: Mutex<Vec<PathBuf>>,
    fail_set_executable: AtomicBool,
    directory_calls: Mutex<Vec<DirectoryCall>>,
    fail_sync_dir_of: Mutex<Option<PathBuf>>,
}

/// What [`RecordingHost`] saw done to directories, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryCall {
    Sync(PathBuf),
    Publish { from: PathBuf, to: PathBuf },
}

impl RecordingHost {
    pub fn new() -> RecordingHost {
        RecordingHost::default()
    }

    /// A host whose file names follow the rules of Windows (forbidden characters, the length limit), whatever OS the
    /// test runs on.
    pub fn with_windows_names() -> RecordingHost {
        RecordingHost {
            windows_names: true,
            ..RecordingHost::default()
        }
    }

    /// A host that cannot make symlinks, as a Windows host without the privilege: `make_symlink` fails with
    /// `PermissionDenied`.
    pub fn without_symlinks() -> RecordingHost {
        RecordingHost {
            no_symlinks: true,
            ..RecordingHost::default()
        }
    }

    /// A host that says it makes the two kinds of symlink differently (`kinds`, as Windows does) or not (as Unix does),
    /// whatever OS the test runs on.
    pub fn with_link_kinds(kinds: bool) -> RecordingHost {
        RecordingHost {
            link_kinds: Some(kinds),
            ..RecordingHost::default()
        }
    }

    /// The `to_dir` of every `make_symlink` call, in call order. A copy makes a link under a temporary name, so the
    /// order is how a call is told from another.
    pub fn symlinks_made(&self) -> Vec<bool> {
        self.symlinks_made.lock().unwrap().clone()
    }

    /// Makes the `index`-th (from 0) call of `write_private` fail.
    pub fn fail_private_write(&self, index: usize) {
        *self.fail_private_write_at.lock().unwrap() = Some(index);
    }

    /// Makes `sync_dir` fail for `dir`.
    pub fn fail_sync_dir(&self, dir: &Path) {
        *self.fail_sync_dir_of.lock().unwrap() = Some(dir.to_owned());
    }

    /// Makes every `set_executable` fail.
    pub fn fail_set_executable(&self) {
        self.fail_set_executable.store(true, Ordering::SeqCst);
    }

    /// Makes `is_executable` say yes for `path`.
    pub fn mark_executable(&self, path: &Path) {
        self.executable.lock().unwrap().insert(path.to_owned());
    }

    /// The paths given to `write_private`, in call order, failed calls included.
    pub fn private_writes(&self) -> Vec<PathBuf> {
        self.private_writes.lock().unwrap().clone()
    }

    /// The `sync_dir` and `publish_dir` calls, in call order.
    pub fn directory_calls(&self) -> Vec<DirectoryCall> {
        self.directory_calls.lock().unwrap().clone()
    }

    pub fn set_executable_calls(&self) -> Vec<PathBuf> {
        self.set_executable_calls.lock().unwrap().clone()
    }
}

impl Host for RecordingHost {
    fn accelerator(&self, arch: Arch) -> std::result::Result<Accel, Unavailable> {
        TestHost.accelerator(arch)
    }

    fn guest_reset(&self, arch: Arch) -> std::result::Result<(), Unavailable> {
        TestHost.guest_reset(arch)
    }

    fn socket_path_max(&self) -> usize {
        TestHost.socket_path_max()
    }

    fn watch_process(&self, id: ProcessId) -> Result<Arc<dyn ProcessWatch>> {
        TestHost.watch_process(id)
    }

    fn ansi_code_page(&self) -> Option<u32> {
        TestHost.ansi_code_page()
    }

    fn detach(&self, cmd: &mut cosca::Command) -> Result<()> {
        TestHost.detach(cmd)
    }

    fn exit_width(&self) -> ExitWidth {
        TestHost.exit_width()
    }

    fn quiet(&self, cmd: &mut std::process::Command) {
        TestHost.quiet(cmd)
    }

    fn env_name_strip_prefix(&self, name: &OsStr, prefix: &str) -> Option<String> {
        TestHost.env_name_strip_prefix(name, prefix)
    }

    fn system_dirs(&self) -> Option<SystemDirs> {
        TestHost.system_dirs()
    }

    fn write_private(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let index = {
            let mut writes = self.private_writes.lock().unwrap();
            writes.push(path.to_owned());
            writes.len() - 1
        };
        if *self.fail_private_write_at.lock().unwrap() == Some(index) {
            return Err(std::io::Error::other("the test made this private write fail"));
        }
        TestHost.write_private(path, bytes)
    }

    fn is_executable(&self, path: &Path) -> std::io::Result<bool> {
        if self.executable.lock().unwrap().contains(path) {
            return Ok(true);
        }
        TestHost.is_executable(path)
    }

    fn set_executable(&self, path: &Path) -> std::io::Result<()> {
        self.set_executable_calls.lock().unwrap().push(path.to_owned());
        if self.fail_set_executable.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("the test made this set_executable fail"));
        }
        TestHost.set_executable(path)
    }

    fn set_mtime(&self, path: &Path, seconds: u32) -> std::io::Result<()> {
        TestHost.set_mtime(path, seconds)
    }

    fn file_name_problem(&self, name: &str) -> Option<String> {
        if self.windows_names {
            return names::windows_problem(name);
        }
        TestHost.file_name_problem(name)
    }

    fn replace_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        TestHost.replace_file(from, to)
    }

    fn publish_dir(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.directory_calls.lock().unwrap().push(DirectoryCall::Publish {
            from: from.to_owned(),
            to: to.to_owned(),
        });
        TestHost.publish_dir(from, to)
    }

    fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        self.directory_calls
            .lock()
            .unwrap()
            .push(DirectoryCall::Sync(dir.to_owned()));
        if self.fail_sync_dir_of.lock().unwrap().as_deref() == Some(dir) {
            return Err(std::io::Error::other("the test made this sync_dir fail"));
        }
        TestHost.sync_dir(dir)
    }

    fn read_link(&self, path: &Path) -> std::io::Result<String> {
        TestHost.read_link(path)
    }

    fn symlinks_have_kinds(&self) -> bool {
        self.link_kinds.unwrap_or_else(|| TestHost.symlinks_have_kinds())
    }

    fn make_symlink(&self, target: &str, link: &Path, to_dir: bool) -> std::io::Result<()> {
        self.symlinks_made.lock().unwrap().push(to_dir);
        if self.no_symlinks {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the test host cannot make symlinks",
            ));
        }
        TestHost.make_symlink(target, link, to_dir)
    }

    fn open_entry(&self, root: &Path, rel: &str) -> std::io::Result<Entry> {
        TestHost.open_entry(root, rel)
    }
}

/// What [`NoGuestReset`] says is missing.
pub const WINDOWS_11: &str =
    "rebooting a guest needs Windows 11 or later (this host's WinHvPlatform.dll has no WHvResetPartition)";

/// A host like the real one that cannot reset a running guest (Windows 10).
pub struct NoGuestReset;

pub static NO_GUEST_RESET: NoGuestReset = NoGuestReset;

impl Host for NoGuestReset {
    fn accelerator(&self, arch: Arch) -> std::result::Result<Accel, Unavailable> {
        host::current().accelerator(arch)
    }

    fn guest_reset(&self, _arch: Arch) -> std::result::Result<(), Unavailable> {
        Err(Unavailable {
            missing: WINDOWS_11.to_owned(),
        })
    }

    fn socket_path_max(&self) -> usize {
        host::current().socket_path_max()
    }

    fn watch_process(&self, id: ProcessId) -> Result<Arc<dyn ProcessWatch>> {
        host::current().watch_process(id)
    }

    fn ansi_code_page(&self) -> Option<u32> {
        host::current().ansi_code_page()
    }

    fn detach(&self, cmd: &mut cosca::Command) -> Result<()> {
        host::current().detach(cmd)
    }

    fn exit_width(&self) -> crate::exit::ExitWidth {
        host::current().exit_width()
    }

    fn quiet(&self, cmd: &mut std::process::Command) {
        host::current().quiet(cmd)
    }

    fn env_name_strip_prefix(&self, name: &OsStr, prefix: &str) -> Option<String> {
        host::current().env_name_strip_prefix(name, prefix)
    }

    fn system_dirs(&self) -> Option<SystemDirs> {
        host::current().system_dirs()
    }

    fn write_private(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        host::current().write_private(path, bytes)
    }

    fn is_executable(&self, path: &Path) -> std::io::Result<bool> {
        host::current().is_executable(path)
    }

    fn set_executable(&self, path: &Path) -> std::io::Result<()> {
        host::current().set_executable(path)
    }

    fn set_mtime(&self, path: &Path, seconds: u32) -> std::io::Result<()> {
        host::current().set_mtime(path, seconds)
    }

    fn file_name_problem(&self, name: &str) -> Option<String> {
        host::current().file_name_problem(name)
    }

    fn replace_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        host::current().replace_file(from, to)
    }

    fn publish_dir(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        host::current().publish_dir(from, to)
    }

    fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        host::current().sync_dir(dir)
    }

    fn read_link(&self, path: &Path) -> std::io::Result<String> {
        host::current().read_link(path)
    }

    fn symlinks_have_kinds(&self) -> bool {
        host::current().symlinks_have_kinds()
    }

    fn make_symlink(&self, target: &str, link: &Path, to_dir: bool) -> std::io::Result<()> {
        host::current().make_symlink(target, link, to_dir)
    }

    fn open_entry(&self, root: &Path, rel: &str) -> std::io::Result<Entry> {
        host::current().open_entry(root, rel)
    }
}
