//! Hosts for the tests: one whose accelerator always works, and one that has none.

use std::path::Path;

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
}
