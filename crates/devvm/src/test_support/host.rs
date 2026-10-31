//! Hosts for the tests: one whose accelerator always works, and one that has none.

use std::path::Path;
use std::sync::Arc;

use cosca::identity::ProcessId;

use crate::exit::ExitWidth;
use crate::host::{self, Accel, Host, ProcessWatch, SystemDirs, Unavailable};
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
