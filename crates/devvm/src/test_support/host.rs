//! A host whose accelerator always works, so engine tests run on every machine.

use crate::exit::ExitWidth;
use crate::host::{self, Accel, Host, Unavailable};
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
}
