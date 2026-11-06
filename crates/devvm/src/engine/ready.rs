//! Waiting for a started guest to answer.

use cosca::identity::ProcessIdRecord;

use super::Engine;
use crate::access::{BootBound, Keepalive, SshSession, SshTarget, VmExit};
use crate::contain::VmProcess;
use crate::guest::{GuestOs, Readiness};
use crate::instance::name::InstanceName;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "ready_tests.rs"]
mod ready_tests;

/// Connects to the guest at `target` and waits until its first boot has finished. The wait ends when the guest is
/// ready, when the VM process `process` exits, or when the engine's boot bound passes, whichever comes first; a failure
/// names what was awaited and the serial log. The returned session is past its boot phase.
///
/// A boot that finished with recoverable problems is reported on the progress channel and is still a success.
pub(crate) fn connect_ready(
    engine: &Engine,
    guest: &dyn GuestOs,
    name: &InstanceName,
    target: &SshTarget,
    process: &ProcessIdRecord,
) -> Result<SshSession> {
    let bound = BootBound::start(engine.boot_bound())?;
    let Some(process) = VmProcess::attach(process)? else {
        return Err(Error::new(
            ErrorKind::ConnectionLost,
            format!(
                "waiting for SSH on {} to answer: the VM process exited (serial log: {})",
                target.addr,
                target.serial_log.display()
            ),
        ));
    };
    let session = SshSession::connect(
        target,
        Keepalive::STANDARD,
        VmExit::watch(process)?,
        Some(bound),
        engine.progress().as_ref(),
    )?;
    if let Readiness::Degraded(what) = guest.wait_ready(&session)? {
        engine.progress().message(&format!(
            "instance {name}: the first boot finished with problems: {what}"
        ));
    }
    session.finish_boot();
    Ok(session)
}
