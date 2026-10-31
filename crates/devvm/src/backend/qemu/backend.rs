//! The QEMU backend: launches QEMU frozen from a `MachineSpec` and finds it again from a record.

use std::path::PathBuf;
use std::sync::Arc;

use super::args::{LaunchPaths, build_args, qemu_arch};
use super::machine::{QemuMachine, qemu_log};
use super::qmp::QmpEndpoint;
use super::sidecar::LazyInstall;
use super::storage::QemuStorage;
use crate::backend::{Backend, Machine, MachineRecord, MachineSpec, Storage};
use crate::contain::VmProcess;
use crate::exit::Progress;
use crate::host::Host;
use crate::{Error, ErrorKind, Result};

pub(crate) struct QemuBackend {
    install: Arc<LazyInstall>,
    storage: Arc<QemuStorage>,
    host: &'static dyn Host,
    /// Where control sockets go. Short, because a socket path is limited (`Host::socket_path_max`).
    temp: PathBuf,
    progress: Arc<dyn Progress>,
}

impl QemuBackend {
    pub(crate) fn new(
        install: Arc<LazyInstall>,
        storage: Arc<QemuStorage>,
        host: &'static dyn Host,
        temp: PathBuf,
        progress: Arc<dyn Progress>,
    ) -> QemuBackend {
        QemuBackend {
            install,
            storage,
            host,
            temp,
            progress,
        }
    }
}

impl QemuBackend {
    /// `error`, after the frozen QEMU of a launch that failed has been killed and the files it could have made are
    /// removed. A QEMU that cannot be killed is named in the error: it is still there, frozen.
    fn abandon(&self, vm: &mut VmProcess, endpoint: &QmpEndpoint, error: Error) -> Error {
        for path in [endpoint.path().to_path_buf(), endpoint.lock_path()] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => self.progress.message(&format!("cannot delete {}: {e}", path.display())),
            }
        }
        match vm.kill() {
            Ok(()) => error,
            Err(kill) => Error::new(
                error.kind(),
                format!("{error}; the frozen QEMU could not be killed either ({kill}) and is still running"),
            ),
        }
    }
}

impl Backend for QemuBackend {
    /// Starts QEMU frozen, in an empty private working directory, with nothing in its environment but `SystemRoot` and
    /// a `PATH` of System32 and its own `bin`. Its output goes to `<instance>/qemu.log`. Resolving the sidecar here is
    /// the only place a launch can download it.
    fn launch(&self, spec: &MachineSpec) -> Result<Box<dyn Machine>> {
        let install = self.install.get()?;
        let endpoint = QmpEndpoint::for_instance(&self.temp, &spec.id, self.host.socket_path_max())?;
        let paths = LaunchPaths::resolve(spec, endpoint.path().to_path_buf(), &install.share)?;
        let args = build_args(spec, &paths)?;

        // An earlier run may have left files in the working directory; QEMU starts in an empty one.
        let cwd = spec.dir.join("qemu-cwd");
        match std::fs::remove_dir_all(&cwd) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(ErrorKind::Io, format!("cannot clear {}", cwd.display()), e)),
        }
        std::fs::create_dir_all(&cwd)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", cwd.display()), e))?;

        let mut command = cosca::Command::new();
        command
            .arg(install.tool(&format!("qemu-system-{}", qemu_arch(spec.arch))))
            .args(args)
            .env_clear()
            .envs(install.environment(self.host.system_dirs().as_ref())?)
            .current_dir(&cwd);
        let log = qemu_log(&spec.dir);
        let mut vm = VmProcess::spawn_frozen(self.host, command, spec.lifetime, &log)?;
        let process = match vm.record() {
            Ok(process) => process,
            Err(e) => return Err(self.abandon(&mut vm, &endpoint, e)),
        };
        let watch = match self.host.watch_process(vm.id()) {
            Ok(watch) => watch,
            Err(e) => return Err(self.abandon(&mut vm, &endpoint, e)),
        };

        let record = MachineRecord {
            process,
            qmp: endpoint.path().display().to_string(),
            ssh: Vec::new(),
            serial_log: spec.serial_log.clone(),
            leftovers: vec![endpoint.path().to_path_buf(), endpoint.lock_path()],
            no_reboot: spec.profile.no_reboot,
        };
        Ok(Box::new(QemuMachine::launched(
            vm,
            watch,
            record,
            spec.forwards.clone(),
            log,
            spec.arch,
            self.host,
            self.progress.clone(),
        )))
    }

    fn attach(&self, record: &MachineRecord) -> Result<Box<dyn Machine>> {
        let Some(vm) = VmProcess::attach(&record.process)? else {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("no machine is running as process {}", record.process.pid),
            ));
        };
        let watch = self.host.watch_process(vm.id())?;
        Ok(Box::new(QemuMachine::attached(
            vm,
            watch,
            record.clone(),
            self.host,
            self.progress.clone(),
        )?))
    }

    fn storage(&self) -> &dyn Storage {
        &*self.storage
    }

    /// WHPX does not support saving a running machine's memory.
    fn can_save_memory(&self) -> bool {
        false
    }
}
