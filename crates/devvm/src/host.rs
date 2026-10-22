//! What differs between host operating systems, behind one trait. The only `cfg` in devvm's logic is at the top of this
//! file, where the implementation is chosen.

use std::fmt;

use crate::exit::ExitWidth;
use crate::{Arch, Result};

#[cfg(test)]
#[path = "host_tests.rs"]
mod host_tests;

#[cfg_attr(windows, allow(dead_code))]
mod unsupported;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
use unsupported::UnsupportedHost as Current;
#[cfg(windows)]
use windows::WindowsHost as Current;

/// How a hypervisor is reached on this host.
// The backends that return the other variants arrive with U4.
#[allow(dead_code)]
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

pub(crate) trait Host: Send + Sync {
    /// The accelerator for guests of `arch`, or what is missing.
    fn accelerator(&self, arch: Arch) -> std::result::Result<Accel, Unavailable>;

    /// Starts `cmd` without a console, so no console control event reaches the VM. It never breaks the process away
    /// from an enclosing job: whatever the outer process installed is not devvm's business.
    fn detach(&self, cmd: &mut cosca::Command) -> Result<()>;

    /// How many bits of a guest exit code this host's processes can pass on.
    fn exit_width(&self) -> ExitWidth;
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
