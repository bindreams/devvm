//! Hosts for the tests: one whose accelerator always works, and one that also records what callers ask of it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::exit::ExitWidth;
use crate::host::{self, Accel, Host, SystemDirs, Unavailable};
use crate::{Arch, Result};

/// Reports TCG as the accelerator and delegates everything else to the real host, so the real `detach` is the one the
/// tests exercise.
pub struct TestHost;

impl Host for TestHost {
    fn accelerator(&self, _arch: Arch) -> std::result::Result<Accel, Unavailable> {
        Ok(Accel::Tcg)
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
}

/// What a [`RecordingHost`] was asked to do to a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileCall {
    WritePrivate(PathBuf),
    SetExecutable(PathBuf),
}

/// A [`TestHost`] that records the file-permission calls it receives and reports exactly the files marked with
/// [`RecordingHost::mark_executable`] as executable, so a test means the same on every OS. Writes and
/// `set_executable` still reach the real host. Hand it to code that wants a `&'static dyn Host` with `Box::leak`.
#[derive(Default)]
#[allow(dead_code, reason = "U4's access and recipe tests are the first users")]
pub struct RecordingHost {
    calls: Mutex<Vec<FileCall>>,
    executable: Mutex<HashSet<PathBuf>>,
}

#[allow(dead_code, reason = "U4's access and recipe tests are the first users")]
impl RecordingHost {
    pub fn new() -> RecordingHost {
        RecordingHost::default()
    }

    /// Makes `is_executable` report `path` as executable.
    pub fn mark_executable(&self, path: &Path) {
        self.executable.lock().unwrap().insert(path.to_path_buf());
    }

    /// The file-permission calls so far, in order.
    pub fn calls(&self) -> Vec<FileCall> {
        self.calls.lock().unwrap().clone()
    }
}

impl Host for RecordingHost {
    fn accelerator(&self, arch: Arch) -> std::result::Result<Accel, Unavailable> {
        TestHost.accelerator(arch)
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

    fn system_dirs(&self) -> Option<SystemDirs> {
        TestHost.system_dirs()
    }

    fn write_private(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(FileCall::WritePrivate(path.to_path_buf()));
        TestHost.write_private(path, bytes)
    }

    fn is_executable(&self, path: &Path) -> std::io::Result<bool> {
        Ok(self.executable.lock().unwrap().contains(path))
    }

    fn set_executable(&self, path: &Path) -> std::io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(FileCall::SetExecutable(path.to_path_buf()));
        TestHost.set_executable(path)
    }
}
