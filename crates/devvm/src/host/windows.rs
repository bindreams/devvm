//! The Windows host. U4 replaces the accelerator stub with WHPX detection.

use super::{Accel, Host, Unavailable};
use crate::exit::ExitWidth;
use crate::{Arch, Result};

pub(super) struct WindowsHost;

impl Host for WindowsHost {
    fn accelerator(&self, _arch: Arch) -> std::result::Result<Accel, Unavailable> {
        Err(Unavailable {
            missing: "WHPX detection arrives with the QEMU backend".to_owned(),
        })
    }

    /// `DETACHED_PROCESS`: the VM has no console, so no console control event can reach it.
    fn detach(&self, cmd: &mut cosca::Command) -> Result<()> {
        cmd.detached();
        Ok(())
    }

    fn exit_width(&self) -> ExitWidth {
        ExitWidth::Full32
    }
}
