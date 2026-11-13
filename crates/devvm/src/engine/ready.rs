use super::Engine;
use crate::access::session::RETRY_PAUSE;
use crate::access::{BootBound, Keepalive, SshSession, SshTarget, VmExit};
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
    let session = SshSession::connect(
        target,
        Keepalive::STANDARD,
        vm.clone(),
        Some(bound),
        engine.progress().as_ref(),
    )?;
    ready_session(engine, guest, name, target, &vm, bound, session)
}

/// Waits for the guest's readiness on `session`, past its boot phase. A guest that restarts during its boot ends the
/// connection: that is not an end of the wait while the VM lives and the boot bound has not passed, so the wait goes
/// on over a new connection, which reaches the new boot. Any other failure ends it.
fn ready_session(
    engine: &Engine,
    guest: &dyn GuestOs,
    name: &InstanceName,
    target: &SshTarget,
    vm: &VmExit,
    bound: BootBound,
    mut session: SshSession,
) -> Result<SshSession> {
    loop {
        match wait_ready(engine, guest, name, &session) {
            Ok(()) => {
                session.finish_boot();
                return Ok(session);
            }
            Err(e) if e.kind() == ErrorKind::ConnectionLost && !bound.has_passed() && !vm.has_ended() => {
                session = SshSession::connect(
                    target,
                    Keepalive::STANDARD,
                    vm.clone(),
                    Some(bound),
                    engine.progress().as_ref(),
                )?;
            }
            Err(e) => return Err(e),
        }
    }
}

/// How a read of the boot id went, once the command ran.
pub(crate) enum ReadBootIdOutcome {
    Id(String),
    /// The command ended with a status other than 0, or printed no id.
    CommandFailed(Error),
}

impl ReadBootIdOutcome {
    /// The boot id; a command that failed is its error.
    pub(crate) fn into_id(self) -> Result<String> {
        match self {
            ReadBootIdOutcome::Id(id) => Ok(id),
            ReadBootIdOutcome::CommandFailed(error) => Err(error),
        }
    }
}

/// The boot id of the guest `session` is connected to. An error is a read that could not run (`connection-lost`,
/// `no-exit-status`, `io`).
pub(crate) fn read_boot_id(guest: &dyn GuestOs, session: &SshSession) -> Result<ReadBootIdOutcome> {
    let command = guest.wrap(&guest.boot_id_command())?;
    let output = session.exec_collect(&command)?;
    if output.status != ExitStatus::Code(0) {
        return Ok(ReadBootIdOutcome::CommandFailed(Error::new(
            ErrorKind::Io,
            format!(
                "reading the guest's boot id with `{command}` ended with {:?}: {}",
                output.status,
                output.stderr_lossy().trim()
            ),
        )));
    }
    let id = output.stdout_lossy().trim().to_owned();
    if id.is_empty() {
        return Ok(ReadBootIdOutcome::CommandFailed(Error::new(
            ErrorKind::Io,
            format!("reading the guest's boot id with `{command}` printed no boot id"),
        )));
    }
    Ok(ReadBootIdOutcome::Id(id))
}

/// After the guest of `machine` was reset: connects until the guest reports a boot id other than `old_id`, each read
/// on a new connection (one that reached the old guest cannot see the new boot), then waits until its boot has
/// finished. A read that fails because the connection was lost, or whose command ended with a status other than 0 or
/// printed no id, counts as one that still says `old_id`: the guest is going down or its `sshd` restarts. Any other
/// failed read fails the reboot at once.
///
/// The wait ends as [`connect_ready`]'s does, and a failure also names the last failed read. Each unsuccessful read is
/// followed by the connection retry's pause.
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
    let mut last_failed_read: Option<Error> = None;
    loop {
        let session = SshSession::connect(
            target,
            Keepalive::STANDARD,
            vm.clone(),
            Some(bound),
            engine.progress().as_ref(),
        )
        .map_err(|e| match &last_failed_read {
            Some(read) => e.together_with(format!("the last read of the boot id failed ({})", read.message())),
            None => e,
        })?;
        last_failed_read = match read_boot_id(guest, &session) {
            Ok(ReadBootIdOutcome::Id(id)) if id != old_id => {
                return ready_session(engine, guest, name, target, &vm, bound, session);
            }
            Ok(ReadBootIdOutcome::Id(_)) => None,
            Ok(ReadBootIdOutcome::CommandFailed(e)) => Some(e),
            Err(e) if e.kind() == ErrorKind::ConnectionLost => Some(e),
            Err(e) => return Err(e),
        };
        if bound.has_passed() {
            return Err(boot_id_unchanged(target, old_id, bound, last_failed_read.as_ref()));
        }
        // The read is asked again, paced as a connection retry is: a guest that answers at once with "not yet" (a
        // Windows guest's session 0 does, with 126) would otherwise be asked in a tight loop.
        std::thread::sleep(RETRY_PAUSE);
    }
}

/// The failure of a boot id that stayed `old_id` (or could not be read, `last_failed_read`) until `bound` passed.
pub(super) fn boot_id_unchanged(
    target: &SshTarget,
    old_id: &str,
    bound: BootBound,
    last_failed_read: Option<&Error>,
) -> Error {
    let read = last_failed_read.map_or(String::new(), |e| format!("the last read failed: {}; ", e.message()));
    Error::new(
        ErrorKind::ConnectionLost,
        format!(
            "waiting for the boot id of {} to differ from {old_id}: it did not happen within the boot bound of {} \
             ({read}serial log: {})",
            target.addr,
            crate::access::session::humanly(bound.limit()),
            target.serial_log.display()
        ),
    )
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
