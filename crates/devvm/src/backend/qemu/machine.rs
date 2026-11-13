//! A QEMU process as a `Machine`: launched frozen, resumed, found again by identity, reached over QMP.
//!
//! QEMU's output goes to `qemu.log` in the instance directory. The file is quoted in errors when the process has exited
//! and is never parsed: a healthy guest reset writes WHPX error lines to it.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::args::guest_port;
use super::qmp::{Qmp, QmpEndpoint, VmEnd};
use crate::backend::{Endpoint, FileClaim, Machine, MachineExit, MachineRecord, SshEndpoint, directory_of};
use crate::contain::VmProcess;
use crate::exit::Progress;
use crate::host::{Host, ProcessWatch};
use crate::{Arch, Error, ErrorKind, Result};

#[cfg(test)]
#[path = "machine_tests.rs"]
mod machine_tests;

#[cfg(all(test, windows))]
#[path = "machine_windows_tests.rs"]
mod machine_windows_tests;

/// The run states in which a guest powers down on request, or is already going.
const POWERDOWN_STATES: &[&str] = &["running", "suspended", "shutdown"];

/// Why a machine launched with `-no-reboot` is not reset.
const NO_REBOOT_REFUSAL: &str = "cannot reset the VM: it runs with -no-reboot, under which QEMU exits at a reset \
                                 instead of resetting the guest";

/// What QEMU wrote to its standard output and error: `qemu.log` in the instance directory.
pub(super) fn qemu_log(dir: &Path) -> PathBuf {
    dir.join("qemu.log")
}

pub(super) struct QemuMachine {
    vm: VmProcess,
    record: MachineRecord,
    /// The record's claim: the file whose identity says the control socket is this machine's (`Qmp::connect`).
    claim: FileClaim,
    ssh: Vec<(Endpoint, SocketAddr)>,
    /// The forwards this machine was launched with; their host ports are read back once it runs.
    forwards: Vec<Endpoint>,
    endpoint: QmpEndpoint,
    /// The process, so that a session blocked on a dead QEMU wakes.
    process: Arc<dyn ProcessWatch>,
    /// Where QEMU's output goes: `launch` was given the directory, `attach` has only the record's serial log.
    log: PathBuf,
    /// Where this launch's output begins in `log`, which every launch in a directory appends to: `launch` knows it, and
    /// an attached machine does not, so it starts at 0.
    log_from: u64,
    /// What the guest is, for the host to say whether it can reset one; `None` for a machine found with `attach`, which
    /// asks QEMU.
    arch: Option<Arch>,
    host: &'static dyn Host,
    progress: Arc<dyn Progress>,
    /// Found with `attach`: already running, and not this process's to contain.
    attached: bool,
    /// Whether QEMU has been let run: a frozen machine has no control socket yet.
    resumed: bool,
}

impl QemuMachine {
    /// A machine this process just launched, frozen. `process` watches `vm`'s process.
    #[allow(
        clippy::too_many_arguments,
        reason = "each is a different fact about the launch; a struct would only rename them"
    )]
    pub(super) fn launched(
        vm: VmProcess,
        process: Arc<dyn ProcessWatch>,
        record: MachineRecord,
        claim: FileClaim,
        forwards: Vec<Endpoint>,
        log: PathBuf,
        log_from: u64,
        arch: Arch,
        host: &'static dyn Host,
        progress: Arc<dyn Progress>,
    ) -> QemuMachine {
        QemuMachine {
            endpoint: QmpEndpoint::from_record(&record.qmp),
            process,
            log,
            log_from,
            arch: Some(arch),
            vm,
            record,
            claim,
            ssh: Vec::new(),
            forwards,
            host,
            progress,
            attached: false,
            resumed: false,
        }
    }

    /// A machine that runs already, found from its record, whose `claim` is the record's.
    pub(super) fn attached(
        vm: VmProcess,
        process: Arc<dyn ProcessWatch>,
        record: MachineRecord,
        claim: FileClaim,
        host: &'static dyn Host,
        progress: Arc<dyn Progress>,
    ) -> Result<QemuMachine> {
        let log = qemu_log(directory_of(&record.serial_log, "the machine's serial log")?);
        Ok(QemuMachine {
            endpoint: QmpEndpoint::from_record(&record.qmp),
            process,
            log,
            log_from: 0,
            arch: None,
            ssh: record.ssh.iter().map(|e| (e.endpoint, e.addr)).collect(),
            forwards: record.ssh.iter().map(|e| e.endpoint).collect(),
            vm,
            record,
            claim,
            host,
            progress,
            attached: true,
            resumed: true,
        })
    }

    /// A session with QEMU. `ConnectionLost` means QEMU is gone or going.
    fn open(&self) -> Result<Qmp> {
        Qmp::connect(self.host, &self.endpoint, &self.claim, &self.process, &*self.progress)
    }

    fn connect(&self) -> Result<Qmp> {
        self.open().map_err(|e| self.explain(e))
    }

    /// `error` with what the process left behind, when the error means QEMU is gone: only a closed control socket
    /// (`ConnectionLost`) is a QEMU on its way out, so this waits for the exit before it reads the log. Any other error
    /// is about the connection and says nothing of the process, which may be alive.
    fn explain(&self, error: Error) -> Error {
        if error.kind() != ErrorKind::ConnectionLost {
            return error;
        }
        if let Err(wait) = self.vm.wait() {
            return wait;
        }
        self.exited(error.message())
    }

    /// What QEMU printed in this launch, which `qemu.log` holds from `log_from` on: its text, or a note in brackets
    /// when it printed nothing or the file cannot be read.
    fn output(&self) -> String {
        let output = match std::fs::read(&self.log) {
            Ok(bytes) => {
                let mine = usize::try_from(self.log_from)
                    .ok()
                    .and_then(|from| bytes.get(from..))
                    .unwrap_or(&bytes);
                String::from_utf8_lossy(mine).trim().to_owned()
            }
            Err(e) => return format!("(it could not be read: {e})"),
        };
        if output.is_empty() {
            "(it printed nothing)".to_owned()
        } else {
            output
        }
    }

    /// The error for a QEMU process that exited while devvm was using it, quoting what it printed in this launch.
    fn exited(&self, what: &str) -> Error {
        Error::new(
            ErrorKind::ConnectionLost,
            format!(
                "QEMU exited: {what}; its output ({}):\n{}\nthe serial log is {}",
                self.log.display(),
                self.output(),
                self.record.serial_log.display()
            ),
        )
    }

    /// `Unavailable` for a machine launched with `-no-reboot`, which a reset would end.
    fn refuse_no_reboot(&self) -> Result<()> {
        if self.record.no_reboot {
            return Err(Error::new(ErrorKind::Unavailable, NO_REBOOT_REFUSAL));
        }
        Ok(())
    }

    /// `Internal` for a machine that was never resumed: its control socket does not exist, so asking it anything
    /// would wait for ever.
    fn require_resumed(&self, what: &str) -> Result<()> {
        if self.resumed {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::Internal,
                format!("cannot {what}: the machine was launched but never resumed"),
            ))
        }
    }

    /// Sends the powerdown request and returns when QEMU closes the session. A session lost at any point, `open` and
    /// `query-status` included, is QEMU gone or going, which the caller confirms by waiting for the process.
    fn power_down(&self) -> Result<()> {
        match self.request_power_down() {
            Ok(VmEnd::Exited) => Ok(()),
            Ok(VmEnd::Paused(state)) => Err(Error::new(
                ErrorKind::Unavailable,
                format!(
                    "the VM stopped ({state}) while it was shutting down; its serial log is {}",
                    self.record.serial_log.display()
                ),
            )),
            Err(e) if e.kind() == ErrorKind::ConnectionLost => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn request_power_down(&self) -> Result<VmEnd> {
        let mut qmp = self.open()?;
        qmp.require_state(POWERDOWN_STATES, "shut the VM down", &self.record.serial_log)?;
        qmp.system_powerdown()?;
        qmp.wait_for_end()
    }

    fn remove_leftovers(&self) {
        let mut warn = |text: String| self.progress.message(&text);
        self.record.remove_leftovers(self.host, &mut warn);
    }

    /// The loopback address QEMU bound for `endpoint`'s forward, from the forwards `info usernet` listed.
    fn address_of(endpoint: Endpoint, forwards: &[super::ports::Forward]) -> Result<SocketAddr> {
        let guest = guest_port(endpoint)?;
        forwards
            .iter()
            .find(|forward| forward.guest == guest)
            .map(|forward| SocketAddr::from((Ipv4Addr::LOCALHOST, forward.host)))
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Internal,
                    format!("QEMU lists no forward to guest port {guest}: {forwards:?}"),
                )
            })
    }
}

impl Machine for QemuMachine {
    fn record(&self) -> MachineRecord {
        self.record.clone()
    }

    fn ssh_endpoints(&self) -> &[(Endpoint, SocketAddr)] {
        &self.ssh
    }

    fn serial_log(&self) -> &Path {
        &self.record.serial_log
    }

    /// Lets QEMU run, waits for its control socket (the greeting proves QEMU finished initialising, so the forwards are
    /// bound) and reads the ports it chose. Fails with the contents of `qemu.log` when the process exits first.
    fn resume(&mut self) -> Result<()> {
        if self.attached {
            return Err(Error::new(
                ErrorKind::Internal,
                "a machine found with attach already runs and cannot be resumed",
            ));
        }
        self.vm.resume()?;
        self.resumed = true;
        let mut qmp = self.connect()?;
        let listed = qmp.forwarded_ports().map_err(|e| self.explain(e))?;
        let mut ssh = Vec::new();
        for endpoint in &self.forwards {
            ssh.push((*endpoint, QemuMachine::address_of(*endpoint, &listed)?));
        }
        self.record.ssh = ssh
            .iter()
            .map(|(endpoint, addr)| SshEndpoint {
                endpoint: *endpoint,
                addr: *addr,
            })
            .collect();
        self.ssh = ssh;
        Ok(())
    }

    fn release(&mut self) -> Result<()> {
        if self.attached { Ok(()) } else { self.vm.release() }
    }

    /// The status with what QEMU printed in this launch: the account of why it ended.
    fn try_wait(&self) -> Result<Option<MachineExit>> {
        let Some(status) = self.vm.try_wait()? else {
            return Ok(None);
        };
        Ok(Some(MachineExit {
            status,
            output: self.output(),
        }))
    }

    /// Resets the guest inside this QEMU. Nothing is sent to the VM for a refusal: a machine launched with `-no-reboot`
    /// (QEMU exits at a reset instead of resetting the guest) and a host that cannot reset a guest are both refused
    /// first. A machine found with `attach` does not know its architecture, which the host needs, so it is refused for
    /// `-no-reboot` before it asks QEMU's `query-target`. A VM that is not running (paused, I/O error) is refused by name.
    fn reset(&mut self) -> Result<()> {
        self.require_resumed("reset the VM")?;
        let arch = match self.arch {
            Some(arch) => arch,
            None => {
                self.refuse_no_reboot()?;
                self.connect()?.target_arch().map_err(|e| self.explain(e))?
            }
        };
        self.host
            .guest_reset(arch)
            .map_err(|unavailable| Error::new(ErrorKind::Unavailable, unavailable.to_string()))?;
        self.refuse_no_reboot()?;
        let mut qmp = self.connect()?;
        qmp.require_state(&["running"], "reset the VM", &self.record.serial_log)
            .map_err(|e| self.explain(e))?;
        qmp.system_reset().map_err(|e| self.explain(e))
    }

    /// Asks the guest to power down and waits for the end of the QEMU process. It never escalates to a kill; a VM that is
    /// paused is an error naming its state, because a paused VM ignores the request. A QEMU that exits while this runs
    /// has done what was asked, and so has one whose control session is lost: that means it is gone or going. A guest
    /// that acknowledges the request and never powers off is waited for without end.
    fn shutdown(&mut self) -> Result<()> {
        self.require_resumed("shut the VM down")?;
        self.power_down()?;
        // The session, and with it the lock, is gone before the files are deleted.
        self.vm.wait()?;
        self.remove_leftovers();
        Ok(())
    }

    fn kill(&mut self) -> Result<()> {
        self.vm.kill()?;
        self.remove_leftovers();
        Ok(())
    }
}
