//! The host for operating systems devvm 0.1.0 does not run VMs on (Linux, macOS).

use super::{Accel, Host, Unavailable};
use crate::exit::ExitWidth;
use crate::{Arch, Result};

pub(super) struct UnsupportedHost;

impl Host for UnsupportedHost {
    fn accelerator(&self, _arch: Arch) -> std::result::Result<Accel, Unavailable> {
        Err(Unavailable {
            missing: format!("{} hosts are not supported in devvm 0.1.0", std::env::consts::OS),
        })
    }

    /// A plain spawn already outlives its parent; the host is refused earlier by `accelerator`.
    fn detach(&self, _cmd: &mut cosca::Command) -> Result<()> {
        Ok(())
    }

    fn exit_width(&self) -> ExitWidth {
        ExitWidth::LowByte
    }
}
