//! Waiting for a started or reset guest to answer.

use super::Engine;
use crate::access::{BootBound, Keepalive, SshSession, SshTarget};
use crate::backend::MachineRecord;
use crate::exit::ExitStatus;
use crate::guest::{GuestOs, Readiness};
use crate::instance::name::InstanceName;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "ready_tests.rs"]
mod ready_tests;

/// Connects to the guest at `target` and waits until its boot has finished. The wait ends when the guest is
/// ready, when the VM process of `machine` exits, or when the engine's boot bound passes, whichever comes first; a
/// failure names what was awaited and the serial log. The returned session is past its boot phase.
///
/// A boot that finished with recoverable problems is reported on the progress channel and is still a success.
pub(crate) fn connect_ready(
    engine: &Engine,
    guest: &dyn GuestOs,
    name: &InstanceName,
    target: &SshTarget,
    machine: &MachineRecord,
) -> Result<SshSession> {
    let bound = BootBound::start(engine.boot_bound())?;
    let vm = engine.vm_exit(machine).map_err(|e| awaiting_ssh(e, target))?;
    let session = SshSession::connect(target, Keepalive::STANDARD, vm, Some(bound), engine.progress().as_ref())?;
    wait_ready(engine, guest, name, &session)?;
    session.finish_boot();
    Ok(session)
}

/// The boot id of the guest `session` is connected to.
pub(crate) fn read_boot_id(guest: &dyn GuestOs, session: &SshSession) -> Result<String> {
    let command = guest.wrap(&guest.boot_id_command())?;
    let output = session.exec_collect(&command)?;
    if output.status != ExitStatus::Code(0) {
        return Err(Error::new(
            ErrorKind::Io,
            format!(
                "reading the guest's boot id with `{command}` ended with {:?}: {}",
                output.status,
                output.stderr_lossy().trim()
            ),
        ));
    }
    Ok(output.stdout_lossy().trim().to_owned())
}

/// After the guest of `machine` was reset: connects until the guest reports a boot id other than `old_id`, each read
/// on a new connection (one that reached the old guest cannot see the new boot), then waits until its boot has
/// finished. The wait ends when the new boot is ready, when the VM process exits, or when the engine's boot bound
/// passes; a failure names what was awaited and the serial log. The returned session is past its boot phase.
pub(crate) fn reconnect_after_reset(
    engine: &Engine,
    guest: &dyn GuestOs,
    name: &InstanceName,
    target: &SshTarget,
    machine: &MachineRecord,
    old_id: &str,
) -> Result<SshSession> {
    let bound = BootBound::start(engine.boot_bound())?;
    let vm = engine.vm_exit(machine).map_err(|e| awaiting_ssh(e, target))?;
    loop {
        let session = SshSession::connect(
            target,
            Keepalive::STANDARD,
            vm.clone(),
            Some(bound),
            engine.progress().as_ref(),
        )?;
        if read_boot_id(guest, &session)? != old_id {
            wait_ready(engine, guest, name, &session)?;
            session.finish_boot();
            return Ok(session);
        }
        if bound.has_passed() {
            return Err(Error::new(
                ErrorKind::ConnectionLost,
                format!(
                    "waiting for the boot id of {} to differ from {old_id}: it did not happen within the boot bound \
                     of {} (serial log: {})",
                    target.addr,
                    crate::access::session::humanly(bound.limit()),
                    target.serial_log.display()
                ),
            ));
        }
    }
}

/// The guest's readiness wait on `session`, reporting a boot that finished with problems.
fn wait_ready(engine: &Engine, guest: &dyn GuestOs, name: &InstanceName, session: &SshSession) -> Result<()> {
    if let Readiness::Degraded(what) = guest.wait_ready(session)? {
        engine
            .progress()
            .message(&format!("instance {name}: the boot finished with problems: {what}"));
    }
    Ok(())
}

fn awaiting_ssh(error: Error, target: &SshTarget) -> Error {
    let kind = error.kind();
    error.with_context(kind, format!("waiting for SSH on {} to answer", target.addr))
}
