//! Guest operations over an SSH session: what `Instance` and `Vm` share once each has a session to its guest.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use crate::access::{ExecEnd, Keepalive, SshSession};
use crate::engine::Engine;
use crate::engine::keys::{Running, read_target};
use crate::engine::ready::{read_boot_id, reconnect_after_reset};
use crate::exit::{ExitStatus, Output};
use crate::guest::GuestOs;
use crate::instance::dir::{InstanceDir, OpExclusive, OpHeld};
use crate::{Command, Error, ErrorKind, Result};

#[cfg(test)]
#[path = "ops_tests.rs"]
pub(crate) mod ops_tests;

pub(super) fn guest_of(engine: &Engine, dir: &InstanceDir) -> Result<Arc<dyn GuestOs>> {
    engine.guests().os(dir.record()?.guest)
}

pub(super) fn run(guest: &dyn GuestOs, session: &SshSession, cmd: &Command) -> Result<Output> {
    session.exec_collect(&guest.wrap(cmd)?)
}

/// Runs `cmd`, writing its output to `out` and `err` as it arrives. The command's status is the result: what could not
/// be carried after it (`FinishedWithLoss`) goes to the progress channel, and a host writer that closed is an `io`
/// error.
pub(super) fn run_to(
    engine: &Engine,
    guest: &dyn GuestOs,
    session: &SshSession,
    cmd: &Command,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<ExitStatus> {
    match session.exec(&guest.wrap(cmd)?, out, err)? {
        ExecEnd::Finished(status) => Ok(status),
        ExecEnd::FinishedWithLoss { status, lost } => {
            engine.progress().message(lost.message());
            Ok(status)
        }
        ExecEnd::HostOutputClosed { error, status: None } => Err(Error::io(
            ErrorKind::Io,
            "writing the guest command's output failed, so the guest command was stopped",
            error,
        )),
        ExecEnd::HostOutputClosed {
            error,
            status: Some(status),
        } => Err(Error::io(
            ErrorKind::Io,
            format!(
                "writing the guest command's output failed after the guest command ended with {}",
                describe(&status)
            ),
            error,
        )),
    }
}

fn describe(status: &ExitStatus) -> String {
    match status {
        ExitStatus::Code(code) => format!("exit code {code}"),
        ExitStatus::Signal { name, .. } => format!("signal {name}"),
    }
}

pub(super) fn copy_in(
    engine: &Engine,
    guest: &Arc<dyn GuestOs>,
    session: &SshSession,
    src: &Path,
    dest: &str,
) -> Result<()> {
    session
        .sftp()?
        .copy_in(src, dest, engine.host(), guest, engine.progress().as_ref())
}

pub(super) fn copy_out(
    engine: &Engine,
    guest: &Arc<dyn GuestOs>,
    session: &SshSession,
    src: &str,
    dest: &Path,
) -> Result<()> {
    session
        .sftp()?
        .copy_out(src, dest, engine.host(), guest, engine.progress().as_ref())
}

/// The machine of `dir` and a new connection to it: a single attempt, so a failure is final. (A guest that is still
/// booting is `ready::reconnect_after_reset`'s.)
pub(super) fn reach(engine: &Engine, dir: &InstanceDir, op: &impl OpHeld) -> Result<(Running, SshSession)> {
    let running = read_target(engine, dir, op)?;
    let session = SshSession::connect(
        &running.target,
        Keepalive::STANDARD,
        engine.vm_exit(&running.record)?,
        None,
        engine.progress().as_ref(),
    )?;
    Ok((running, session))
}

/// Resets the running guest of `dir` in place and returns the session to the new boot, once the guest has booted and
/// is ready. The new boot is the one whose boot id differs from the old one's, read on a connection of its own. The
/// caller holds the exclusive op lock. A failure after the reset leaves the machine running.
pub(super) fn reboot(engine: &Engine, dir: &InstanceDir, op: &OpExclusive) -> Result<SshSession> {
    let guest = guest_of(engine, dir)?;
    let (mut running, before) = reach(engine, dir, op)?;
    let old_id = read_boot_id(&*guest, &before)?.into_id()?;
    running.machine.reset()?;
    // The connection to the old boot is no use to anyone now.
    drop(before);
    reconnect_after_reset(engine, &*guest, dir.name(), &running.target, &running.record, &old_id)
}
