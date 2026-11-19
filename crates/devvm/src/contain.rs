//! VM processes: found again by identity (PID plus start token), never by a bare PID.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

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

/// How long a VM process may live relative to the devvm process that started it. The lifetime is the permission and
/// `VmProcess::release` the switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lifetime {
    /// Dies with its owner, enforced by the OS. `release` refuses it.
    DiesWithOwner,
    /// May outlive its owner: detached, with no inherited stdio, and `release` disarms its containment once its
    /// record is written.
    OutlivesOwner,
}

/// A VM's process. It is always reached through its recorded identity, so a recycled PID is never mistaken for it.
///
/// A VM this process starts goes `Frozen` (it exists, nothing of it has run), `Running` (still contained: it dies with
/// this process) and `Released` (containment disarmed: it outlives this process). Dropping a frozen or running one
/// kills it; dropping a released or foreign one leaves it alone.
pub(crate) struct VmProcess {
    /// Read when the process was started or found; every later act on the process opens it by this.
    id: ProcessId,
    state: State,
}

enum State {
    Frozen(cosca::SuspendedChild, Lifetime),
    Running(cosca::Child, Lifetime),
    /// Started by this process, containment disarmed.
    Released,
    /// A process this one did not spawn (or no longer owns).
    Foreign,
}

/// Waits for the process `id` names to exit. A process that is gone has exited.
fn wait_for_exit(id: ProcessId) -> std::result::Result<(), cosca::error::Error> {
    match cosca::Process::open(id)? {
        Some(process) => process.wait(),
        None => Ok(()),
    }
}

/// Kills the process `id` names and waits for it to exit, through one handle: a kill that landed is never reported
/// as failed by a second open that finds the process gone or refuses it. A process that is gone is `Ok`.
fn kill_and_wait(id: ProcessId) -> std::result::Result<(), cosca::error::Error> {
    match cosca::Process::open(id)? {
        Some(process) => {
            process.kill()?;
            process.wait()
        }
        None => Ok(()),
    }
}

/// A cosca failure as an `Io` error that says `context` and keeps the failure as its source.
pub(crate) fn cosca_error(context: &str, source: cosca::error::Error) -> Error {
    Error::new(ErrorKind::Io, format!("{context}: {}", with_causes(&source))).with_source(source)
}

/// `error`'s message followed by each cause's: cosca keeps the cause of a wrapped failure (a resume that failed, then
/// its teardown) as a source, not in the message. A cause whose text the message already holds is not repeated.
fn with_causes(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut cause = error.source();
    while let Some(next) = cause {
        let text = next.to_string();
        if !message.contains(&text) {
            message = format!("{message}: {text}");
        }
        cause = next.source();
    }
    message
}

/// How a failed frozen spawn is reported: a host that cannot freeze a process is `NotSupported`, anything else `Io`.
pub(crate) fn frozen_spawn_error(error: cosca::error::Error) -> Error {
    match error {
        cosca::error::Error::Unsupported {
            op, platform, detail, ..
        } if op == "spawn_suspended" => Error::new(
            ErrorKind::NotSupported,
            format!("cannot start the VM: frozen spawn needs Windows in 0.1.0 (this host is {platform}: {detail})"),
        ),
        other => cosca_error("cannot start the VM process frozen", other),
    }
}

/// What a VM process is started with besides its program. The rest (argv[0], stdio, handles, containment, the
/// console) is [`VmProcess::spawn_frozen`]'s.
pub(crate) struct VmCommand {
    pub(crate) args: Vec<OsString>,
    /// Start from an empty environment instead of this process's.
    pub(crate) clear_env: bool,
    pub(crate) env: Vec<(OsString, OsString)>,
    pub(crate) cwd: Option<PathBuf>,
}

impl VmProcess {
    /// Starts the program at `program` frozen: the process exists and has an identity, and has not run. Record the
    /// identity (`record`), then `resume`. Dropping a frozen `VmProcess` kills the process, so it never runs.
    ///
    /// The process is always contained (killed when this process dies), has no console (the host's detach) and
    /// inherits none of this process's handles or pipes: stdin is null, and stdout and stderr are appended to the file
    /// `log`, which is created. Both lifetimes start alike; `release` is what lets an `OutlivesOwner` VM outlive its
    /// owner.
    ///
    /// `program` is the absolute path of the executable, and argv[0]; `command` gives the rest of the arguments, the
    /// environment and the working directory. Naming the executable is what gives the handle guarantee: cosca 0.8.0's
    /// `routes_to_raw_backend` sends a Windows command that names one to its raw backend, which hands the child only
    /// the handles listed for it. Any other command goes through `std`, which has the child inherit every inheritable
    /// handle of this process.
    ///
    /// Windows only in 0.1.0: cosca can hold a process before its first instruction there alone (a POSIX child cannot
    /// be held before exec without a gate), so every other host gets `NotSupported`.
    pub(crate) fn spawn_frozen(
        host: &dyn Host,
        program: &Path,
        command: VmCommand,
        lifetime: Lifetime,
        log: &Path,
    ) -> Result<VmProcess> {
        if !program.is_absolute() {
            return Err(Error::new(
                ErrorKind::Internal,
                format!("the VM program {} is not an absolute path", program.display()),
            ));
        }
        let mut cmd = cosca::Command::new();
        cmd.arg(program).args(command.args);
        if command.clear_env {
            cmd.env_clear();
        }
        cmd.envs(command.env);
        if let Some(cwd) = command.cwd {
            cmd.current_dir(cwd);
        }
        cmd.executable(program).contain();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot open the VM log {}", log.display()), e))?;
        cmd.fd(cosca::Fd::STDIN, cosca::Stdio::null())
            .and_then(|cmd| cmd.fd(cosca::Fd::STDOUT, cosca::Stdio::from_file(file)))
            .and_then(|cmd| cmd.fd(cosca::Fd::STDERR, cosca::Stdio::merge(cosca::Fd::STDOUT)))
            .map_err(|e| cosca_error("cannot set up the VM process's stdio", e))?;
        host.detach(&mut cmd)?;
        let frozen = cmd.spawn_suspended().map_err(frozen_spawn_error)?;
        // Dropping `frozen` kills the process, so one without an identity never runs.
        let Some(id) = frozen.id() else {
            return Err(Error::new(
                ErrorKind::Io,
                format!(
                    "cannot start the VM: the OS refused the identity of the VM process {}",
                    frozen.pid()
                ),
            ));
        };
        Ok(VmProcess {
            id,
            state: State::Frozen(frozen, lifetime),
        })
    }

    /// Lets a frozen process run; it stays contained. A failed resume leaves no process running. `Internal` unless
    /// the process is frozen.
    pub(crate) fn resume(&mut self) -> Result<()> {
        let pid = self.id().pid();
        let Some((frozen, lifetime)) = self.take_frozen() else {
            return Err(Error::new(
                ErrorKind::Internal,
                format!("cannot resume the VM process {pid}: it is not frozen"),
            ));
        };
        let child = frozen
            .resume()
            .map_err(|e| cosca_error(&format!("cannot resume the VM process {pid}"), e))?;
        self.state = State::Running(child, lifetime);
        Ok(())
    }

    /// Disarms the containment of a resumed process: it now outlives this one. `Internal` unless the process is
    /// running and contained, so a release before `resume`, or a second one, is a mistake; `Internal` too for a VM
    /// started `DiesWithOwner`, whose lifetime does not allow it. The lifetime is the permission, `release` the switch.
    /// A disarm the OS refuses is an error, and the process stays contained.
    pub(crate) fn release(&mut self) -> Result<()> {
        let id = self.id();
        match &self.state {
            State::Running(_, Lifetime::OutlivesOwner) => {}
            State::Running(_, Lifetime::DiesWithOwner) => {
                return Err(Error::new(
                    ErrorKind::Internal,
                    format!(
                        "cannot release the VM process {}: it was started DiesWithOwner, so it must not outlive \
                         its owner",
                        id.pid()
                    ),
                ));
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::Internal,
                    format!(
                        "cannot release the VM process {}: it is not running under this process's containment",
                        id.pid()
                    ),
                ));
            }
        }
        let State::Running(child, lifetime) = std::mem::replace(&mut self.state, State::Released) else {
            unreachable!("checked just above");
        };
        // A refused disarm hands the child back still armed: the process stays contained and `Running`.
        child.detach().map_err(|(child, e)| {
            self.state = State::Running(child, lifetime);
            cosca_error(&format!("cannot release the VM process {}", id.pid()), e)
        })
    }

    fn take_frozen(&mut self) -> Option<(cosca::SuspendedChild, Lifetime)> {
        if !matches!(self.state, State::Frozen(..)) {
            return None;
        }
        // What is left behind is the identity alone: if the resume fails nothing of the process is still held.
        let left = State::Foreign;
        match std::mem::replace(&mut self.state, left) {
            State::Frozen(frozen, lifetime) => Some((frozen, lifetime)),
            _ => unreachable!("checked just above"),
        }
    }

    /// The process `record` names, if it is still running. `None` when it has exited, when the PID now belongs to
    /// another process, or when the record was written on another boot or platform: it names nothing that exists
    /// now. An error when liveness cannot be established, including a record this build cannot read: it may name a
    /// process that still runs.
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
            Err(cosca::error::Error::IdentityRecord {
                kind: RecordErrorKind::ForeignBootSession | RecordErrorKind::ForeignPlatform,
                ..
            }) => return Ok(None),
            Err(error @ cosca::error::Error::IdentityRecord { kind, .. }) => {
                return Err(unreadable_record(record, kind, error));
            }
            Err(other) => {
                return Err(cosca_error(
                    &format!("cannot restore the VM process {}", record.pid),
                    other,
                ));
            }
        };
        match id.is_alive() {
            Liveness::Alive => Ok(Some(VmProcess {
                id,
                state: State::Foreign,
            })),
            Liveness::Dead => Ok(None),
            Liveness::Unknown => Err(unknown_liveness(id)),
        }
    }

    pub(crate) fn id(&self) -> ProcessId {
        self.id
    }

    /// The persistable identity of this process. Valid while the process is still frozen.
    pub(crate) fn record(&self) -> Result<ProcessIdRecord> {
        let id = self.id();
        id.to_record()
            .map_err(|e| cosca_error(&format!("cannot record the VM process {}", id.pid()), e))
    }

    /// Blocks until the process has exited, with no time limit.
    pub(crate) fn wait(&self) -> Result<()> {
        let id = self.id();
        wait_for_exit(id).map_err(|e| cosca_error(&format!("cannot wait for the VM process {}", id.pid()), e))
    }

    /// The exit status of the process if it has exited, `None` while it runs: std's `Child::try_wait`. It reads the
    /// status from the child this process started, so an exit that this process did not see through its own handle
    /// cannot be told from here: `Internal` for a frozen process, for a released one and for one found with `attach`.
    pub(crate) fn try_wait(&self) -> Result<Option<std::process::ExitStatus>> {
        let id = self.id();
        match &self.state {
            State::Running(child, _) => child.try_wait().map_err(|e| {
                cosca_error(
                    &format!("cannot read the exit status of the VM process {}", id.pid()),
                    e,
                )
            }),
            State::Frozen(..) | State::Released | State::Foreign => Err(Error::new(
                ErrorKind::Internal,
                format!(
                    "cannot read the exit status of the VM process {}: this process holds no running child of it",
                    id.pid()
                ),
            )),
        }
    }

    /// Kills the process and returns only after it has exited. A frozen process never runs. An already-dead process
    /// is `Ok`.
    pub(crate) fn kill(&mut self) -> Result<()> {
        let id = self.id();
        let killed = match &self.state {
            State::Frozen(..) => {
                // Killed by identity first, so that a failure is seen: dropping a frozen child kills it too, but
                // discards how that went.
                let killed = kill_and_wait(id);
                self.state = State::Foreign;
                killed
            }
            // `kill_tree` returns once the root has exited.
            State::Running(child, _) => child.kill_tree(),
            State::Released | State::Foreign => kill_and_wait(id),
        };
        killed.map_err(|e| cosca_error(&format!("cannot kill the VM process {}", id.pid()), e))
    }
}

/// A record the identity layer refuses for a reason other than another boot or platform: what it names cannot be told
/// from here, so it is not read as "not running", which would let a second VM start on a disk the first still holds.
fn unreadable_record(record: &ProcessIdRecord, kind: RecordErrorKind, error: cosca::error::Error) -> Error {
    Error::new(
        ErrorKind::Io,
        format!(
            "cannot tell whether the VM process {} is running: its process record {record:?} is in a format this devvm does \
             not read ({kind}); end that process if it still runs, then remove the instance",
            record.pid
        ),
    )
    .with_source(error)
}

pub(crate) fn unknown_liveness(id: ProcessId) -> Error {
    Error::new(
        ErrorKind::Io,
        format!(
            "cannot tell whether the VM process {} is running: the OS refused the query",
            id.pid()
        ),
    )
}
