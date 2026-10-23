//! VM processes: found again by identity (PID plus start token), never by a bare PID.

use cosca::error::RecordErrorKind;
use cosca::identity::{Liveness, ProcessId, ProcessIdRecord};

use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "contain_tests.rs"]
pub(crate) mod contain_tests;

/// How long a VM process lives relative to the devvm process that started it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lifetime {
    /// Dies with its owner, enforced by the OS.
    DiesWithOwner,
    /// Detached, with no inherited stdio; its containment is disarmed once its record is written.
    OutlivesOwner,
}

/// A VM's process. It is always reached through its recorded identity, so a recycled PID is never mistaken for it.
pub(crate) struct VmProcess {
    state: State,
}

enum State {
    /// A process this one did not spawn (or no longer owns).
    Foreign(cosca::Process),
}

fn cosca_error(context: &str, source: cosca::error::Error) -> Error {
    Error::new(ErrorKind::Io, format!("{context}: {source}")).with_source(source)
}

impl VmProcess {
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

    fn process(&self) -> &cosca::Process {
        match &self.state {
            State::Foreign(process) => process,
        }
    }

    /// The persistable identity of this process.
    pub(crate) fn record(&self) -> Result<ProcessIdRecord> {
        let id = self.process().id();
        id.to_record()
            .map_err(|e| cosca_error(&format!("cannot record the VM process {}", id.pid()), e))
    }

    /// Whether the process is still running; an error when the OS refuses to say (never "dead").
    pub(crate) fn is_alive(&self) -> Result<bool> {
        let process = self.process();
        match process.is_alive() {
            Liveness::Alive => Ok(true),
            Liveness::Dead => Ok(false),
            Liveness::Unknown => Err(unknown_liveness(process.id())),
        }
    }

    /// Blocks until the process has exited, with no time limit.
    pub(crate) fn wait(&self) -> Result<()> {
        let process = self.process();
        process
            .wait()
            .map_err(|e| cosca_error(&format!("cannot wait for the VM process {}", process.id().pid()), e))
    }

    /// Kills the process and returns only after it has exited. An already-dead process is `Ok`.
    pub(crate) fn kill(&mut self) -> Result<()> {
        let process = self.process();
        let pid = process.id().pid();
        process
            .kill()
            .map_err(|e| cosca_error(&format!("cannot kill the VM process {pid}"), e))?;
        process
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
