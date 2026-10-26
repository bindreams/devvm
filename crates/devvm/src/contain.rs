//! VM processes: found again by identity (PID plus start token), never by a bare PID.

use cosca::error::RecordErrorKind;
use cosca::identity::{Liveness, ProcessId, ProcessIdRecord};

use crate::host::Host;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "contain_tests.rs"]
pub(crate) mod contain_tests;

#[cfg(all(test, windows))]
#[path = "contain_windows_tests.rs"]
mod contain_windows_tests;

#[cfg(all(test, not(windows)))]
#[path = "contain_other_tests.rs"]
mod contain_other_tests;

/// How long a VM process lives relative to the devvm process that started it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lifetime {
    /// Dies with its owner, enforced by the OS.
    DiesWithOwner,
    /// Detached, with no inherited stdio; its containment is disarmed once its record is written.
    OutlivesOwner,
}

/// A VM's process. It is always reached through its recorded identity, so a recycled PID is never mistaken for it.
///
/// A VM this process starts goes `Frozen` (it exists, nothing of it has run), `Running` (still contained: it dies with
/// this process) and `Released` (containment disarmed: it outlives this process). Dropping a frozen or running one
/// kills it; dropping a released or foreign one leaves it alone.
pub(crate) struct VmProcess {
    state: State,
}

enum State {
    Frozen(cosca::SuspendedChild),
    Running(cosca::Child),
    /// Started by this process, containment disarmed.
    Released(cosca::Process),
    /// A process this one did not spawn (or no longer owns).
    Foreign(cosca::Process),
}

fn cosca_error(context: &str, source: cosca::error::Error) -> Error {
    Error::new(ErrorKind::Io, format!("{context}: {source}")).with_source(source)
}

/// How a failed frozen spawn is reported: a host that cannot freeze a process is `NotSupported`, anything else `Io`.
pub(crate) fn frozen_spawn_error(error: cosca::error::Error) -> Error {
    match error {
        cosca::error::Error::Unsupported { op, platform, detail } if op == "spawn_suspended" => Error::new(
            ErrorKind::NotSupported,
            format!("cannot start the VM: frozen spawn needs Windows in 0.1.0 (this host is {platform}: {detail})"),
        ),
        other => cosca_error("cannot start the VM process frozen", other),
    }
}

impl VmProcess {
    /// Starts `cmd` frozen: the process exists and has an identity, and has not run. Record the identity
    /// (`record`), then `resume`. Dropping a frozen `VmProcess` kills the process, so it never runs.
    ///
    /// The process is always contained (killed when this process dies) and has no console (the host's detach).
    /// `OutlivesOwner` also gives it null stdio, so it holds none of the owner's pipes; `release` disarms the
    /// containment.
    ///
    /// Windows only in 0.1.0: cosca can hold a process before its first instruction there alone (a POSIX child cannot
    /// be held before exec without a gate), so every other host gets `NotSupported`.
    pub(crate) fn spawn_frozen(host: &dyn Host, mut cmd: cosca::Command, lifetime: Lifetime) -> Result<VmProcess> {
        cmd.contain();
        if lifetime == Lifetime::OutlivesOwner {
            for fd in [cosca::Fd::STDIN, cosca::Fd::STDOUT, cosca::Fd::STDERR] {
                cmd.fd(fd, cosca::Stdio::null())
                    .map_err(|e| cosca_error("cannot give the VM process null stdio", e))?;
            }
        }
        // Whatever the lifetime, no console window may open when devvm itself has no console.
        host.detach(&mut cmd)?;
        let frozen = cmd.spawn_suspended().map_err(frozen_spawn_error)?;
        Ok(VmProcess {
            state: State::Frozen(frozen),
        })
    }

    /// Lets a frozen process run; it stays contained. A failed resume leaves no process running. `Internal` unless
    /// the process is frozen.
    pub(crate) fn resume(&mut self) -> Result<()> {
        let pid = self.id().pid();
        let Some(frozen) = self.take_frozen() else {
            return Err(Error::new(
                ErrorKind::Internal,
                format!("cannot resume the VM process {pid}: it is not frozen"),
            ));
        };
        self.state = State::Running(
            frozen
                .resume()
                .map_err(|e| cosca_error(&format!("cannot resume the VM process {pid}"), e))?,
        );
        Ok(())
    }

    /// Disarms the containment of a resumed process: it now outlives this one. `Internal` unless the process is
    /// running and contained, so a release before `resume`, or a second one, is a mistake.
    pub(crate) fn release(&mut self) -> Result<()> {
        let id = self.id();
        if !matches!(self.state, State::Running(_)) {
            return Err(Error::new(
                ErrorKind::Internal,
                format!(
                    "cannot release the VM process {}: it is not running under this process's containment",
                    id.pid()
                ),
            ));
        }
        if let State::Running(child) = std::mem::replace(&mut self.state, State::Released(cosca::Process::from_id(id)))
        {
            child.detach();
        }
        Ok(())
    }

    fn take_frozen(&mut self) -> Option<cosca::SuspendedChild> {
        if !matches!(self.state, State::Frozen(_)) {
            return None;
        }
        // What is left behind is the identity alone: if the resume fails nothing of the process is still held.
        let left = State::Foreign(cosca::Process::from_id(self.id()));
        match std::mem::replace(&mut self.state, left) {
            State::Frozen(frozen) => Some(frozen),
            _ => unreachable!("checked just above"),
        }
    }

    /// The process `record` names, if it is still running. `None` when it has exited, when the PID now belongs to
    /// another process, or when the record is from another boot or platform; an error only when liveness cannot be
    /// established.
    pub(crate) fn attach(record: &ProcessIdRecord) -> Result<Option<VmProcess>> {
        let id = match ProcessId::try_from(record) {
            Ok(id) => id,
            Err(cosca::error::Error::IdentityRecord {
                kind: RecordErrorKind::ScopeUnreadable,
                detail,
                ..
            }) => {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("cannot check the VM process {}: {detail}", record.pid),
                ));
            }
            // Another boot, platform or format: the record names nothing that exists now.
            Err(cosca::error::Error::IdentityRecord { .. }) => return Ok(None),
            Err(other) => {
                return Err(cosca_error(
                    &format!("cannot restore the VM process {}", record.pid),
                    other,
                ));
            }
        };
        let process = cosca::Process::from_id(id);
        match process.is_alive() {
            Liveness::Alive => Ok(Some(VmProcess {
                state: State::Foreign(process),
            })),
            Liveness::Dead => Ok(None),
            Liveness::Unknown => Err(unknown_liveness(id)),
        }
    }

    fn id(&self) -> ProcessId {
        match &self.state {
            State::Frozen(frozen) => frozen.id(),
            State::Running(child) => child.id(),
            State::Released(process) | State::Foreign(process) => process.id(),
        }
    }

    /// The persistable identity of this process. Valid while the process is still frozen.
    pub(crate) fn record(&self) -> Result<ProcessIdRecord> {
        let id = self.id();
        id.to_record()
            .map_err(|e| cosca_error(&format!("cannot record the VM process {}", id.pid()), e))
    }

    /// Whether the process is still running; an error when the OS refuses to say (never "dead").
    pub(crate) fn is_alive(&self) -> Result<bool> {
        let id = self.id();
        match cosca::Process::from_id(id).is_alive() {
            Liveness::Alive => Ok(true),
            Liveness::Dead => Ok(false),
            Liveness::Unknown => Err(unknown_liveness(id)),
        }
    }

    /// Blocks until the process has exited, with no time limit.
    pub(crate) fn wait(&self) -> Result<()> {
        let id = self.id();
        cosca::Process::from_id(id)
            .wait()
            .map_err(|e| cosca_error(&format!("cannot wait for the VM process {}", id.pid()), e))
    }

    /// Kills the process and returns only after it has exited. A frozen process never runs. An already-dead process
    /// is `Ok`.
    pub(crate) fn kill(&mut self) -> Result<()> {
        let id = self.id();
        let pid = id.pid();
        let killed = match &self.state {
            State::Frozen(_) => {
                // Dropping a frozen child kills it, whatever `kill_on_drop` says.
                self.state = State::Foreign(cosca::Process::from_id(id));
                Ok(())
            }
            State::Running(child) => child.kill_tree(),
            State::Released(process) | State::Foreign(process) => process.kill(),
        };
        killed.map_err(|e| cosca_error(&format!("cannot kill the VM process {pid}"), e))?;
        cosca::Process::from_id(id)
            .wait()
            .map_err(|e| cosca_error(&format!("cannot wait for the VM process {pid} to exit"), e))
    }
}

fn unknown_liveness(id: ProcessId) -> Error {
    Error::new(
        ErrorKind::Io,
        format!(
            "cannot tell whether the VM process {} is running: the OS refused the query",
            id.pid()
        ),
    )
}
