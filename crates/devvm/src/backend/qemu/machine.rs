//! A QEMU process as a `Machine`: launched frozen, resumed, found again by identity, reached over QMP.
//!
//! QEMU's output goes to `qemu.log` beside the serial log. The file is quoted in errors when the process has exited and is
//! never parsed: a healthy guest reset writes WHPX error lines to it.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::args::guest_port;
use super::qmp::{Qmp, QmpEndpoint, VmEnd};
use crate::backend::{Endpoint, Machine, MachineRecord, SshEndpoint};
use crate::contain::VmProcess;
use crate::exit::Progress;
use crate::host::Host;
use crate::{Error, ErrorKind, Result};

/// The run states in which a guest powers down on request, or is already going.
const POWERDOWN_STATES: &[&str] = &["running", "suspended", "shutdown"];

/// What QEMU wrote to its standard output and error, in the instance directory beside the serial log.
pub(super) fn qemu_log(serial_log: &Path) -> PathBuf {
    serial_log.parent().unwrap_or(Path::new("")).join("qemu.log")
}

pub(super) struct QemuMachine {
    vm: VmProcess,
    record: MachineRecord,
    ssh: Vec<(Endpoint, SocketAddr)>,
    /// The forwards this machine was launched with; their host ports are read back once it runs.
    forwards: Vec<Endpoint>,
    endpoint: QmpEndpoint,
    host: &'static dyn Host,
    progress: Arc<dyn Progress>,
    /// Found with `attach`: already running, and not this process's to contain.
    attached: bool,
    /// Whether QEMU has been let run: a frozen machine has no control socket yet.
    resumed: bool,
}

impl QemuMachine {
    /// A machine this process just launched, frozen.
    pub(super) fn launched(
        vm: VmProcess,
        record: MachineRecord,
        forwards: Vec<Endpoint>,
        host: &'static dyn Host,
        progress: Arc<dyn Progress>,
    ) -> QemuMachine {
        QemuMachine {
            endpoint: QmpEndpoint::from_record(&record.qmp),
            vm,
            record,
            ssh: Vec::new(),
            forwards,
            host,
            progress,
            attached: false,
            resumed: false,
        }
    }

    /// A machine that runs already, found from its record.
    pub(super) fn attached(
        vm: VmProcess,
        record: MachineRecord,
        host: &'static dyn Host,
        progress: Arc<dyn Progress>,
    ) -> QemuMachine {
        QemuMachine {
            endpoint: QmpEndpoint::from_record(&record.qmp),
            ssh: record.ssh.iter().map(|e| (e.endpoint, e.addr)).collect(),
            forwards: record.ssh.iter().map(|e| e.endpoint).collect(),
            vm,
            record,
            host,
            progress,
            attached: true,
            resumed: true,
        }
    }

    fn connect(&self) -> Result<Qmp> {
        // A process whose liveness cannot be established is treated as alive: the connect keeps trying.
        let alive = || self.vm.is_alive().unwrap_or(true);
        Qmp::connect(&self.endpoint, &alive, &*self.progress).map_err(|e| self.explain(e))
    }

    /// `error` with what the process left behind, when the error means QEMU is gone: a closed control socket is a QEMU
    /// on its way out, so this waits for the exit (a real condition) before it reads the log.
    fn explain(&self, error: Error) -> Error {
        if error.kind() != ErrorKind::ConnectionLost {
            return error;
        }
        if let Err(wait) = self.vm.wait() {
            return wait;
        }
        self.exited(error.message())
    }

    /// The error for a QEMU process that exited while devvm was using it, quoting `qemu.log`.
    fn exited(&self, what: &str) -> Error {
        let log = qemu_log(&self.record.serial_log);
        let output = std::fs::read(&log)
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
            .unwrap_or_else(|e| format!("({} could not be read: {e})", log.display()));
        let output = if output.is_empty() {
            "(it printed nothing)".to_owned()
        } else {
            output
        };
        Error::new(
            ErrorKind::ConnectionLost,
            format!(
                "QEMU exited: {what}; its output ({}):\n{output}\nthe serial log is {}",
                log.display(),
                self.record.serial_log.display()
            ),
        )
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

    fn remove_leftovers(&self) {
        let mut warn = |text: String| self.progress.message(&text);
        self.record.remove_leftovers(&mut warn);
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
        let listed = self.connect()?.forwarded_ports().map_err(|e| self.explain(e))?;
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

    /// Resets the guest inside this QEMU. The host is asked first whether it can: a host that cannot fails here with
    /// nothing sent to the VM. A VM that is not running (paused, I/O error) is refused by name.
    fn reset(&mut self) -> Result<()> {
        self.require_resumed("reset the VM")?;
        let mut qmp = self.connect()?;
        let arch = qmp.target_arch()?;
        self.host
            .guest_reset(arch)
            .map_err(|unavailable| Error::new(ErrorKind::Unavailable, unavailable.to_string()))?;
        qmp.require_state(&["running"], "reset the VM", &self.record.serial_log)?;
        qmp.system_reset().map_err(|e| self.explain(e))
    }

    /// Asks the guest to power down and waits for the end of the QEMU process. It never escalates to a kill; a VM that is
    /// paused is an error naming its state, because a paused VM ignores the request.
    fn shutdown(&mut self) -> Result<()> {
        self.require_resumed("shut the VM down")?;
        if !self.vm.is_alive()? {
            self.remove_leftovers();
            return Ok(());
        }
        let mut qmp = self.connect()?;
        qmp.require_state(POWERDOWN_STATES, "shut the VM down", &self.record.serial_log)?;
        qmp.system_powerdown().map_err(|e| self.explain(e))?;
        match qmp.wait_for_end() {
            Ok(VmEnd::Exited) => {}
            Ok(VmEnd::Paused(state)) => {
                return Err(Error::new(
                    ErrorKind::Unavailable,
                    format!(
                        "the VM stopped ({state}) while it was shutting down; its serial log is {}",
                        self.record.serial_log.display()
                    ),
                ));
            }
            Err(e) => return Err(self.explain(e)),
        }
        // The lock is released before the files are deleted.
        drop(qmp);
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
