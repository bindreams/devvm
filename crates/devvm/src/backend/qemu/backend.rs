//! The QEMU backend: launches QEMU frozen from a `MachineSpec` and finds it again from a record.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::args::{LaunchPaths, build_args, qemu_arch};
use super::machine::{QemuMachine, qemu_log};
use super::qmp::{QmpEndpoint, Reservation};
use super::sidecar::{LazyInstall, QemuInstall};
use super::storage::QemuStorage;
use crate::backend::{Backend, FileClaim, Machine, MachineRecord, MachineSpec, Storage, remove_files};
use crate::contain::VmProcess;
use crate::exit::Progress;
use crate::host::{Hold, Host, remove_tree};
use crate::{Error, ErrorKind, Result};

pub(crate) struct QemuBackend {
    install: Arc<LazyInstall>,
    storage: Arc<QemuStorage>,
    host: &'static dyn Host,
    /// Where control sockets go (`Dirs::run`): the user's alone, and short, because a socket path is limited
    /// (`Host::socket_path_max`). It fails only when asked for, so an engine builds on a host that cannot name it and
    /// only a launch or a sweep says so.
    run: Result<PathBuf>,
    progress: Arc<dyn Progress>,
}

impl QemuBackend {
    pub(crate) fn new(
        install: Arc<LazyInstall>,
        storage: Arc<QemuStorage>,
        host: &'static dyn Host,
        run: Result<PathBuf>,
        progress: Arc<dyn Progress>,
    ) -> QemuBackend {
        QemuBackend {
            install,
            storage,
            host,
            run,
            progress,
        }
    }
}

impl QemuBackend {
    pub(super) fn host(&self) -> &'static dyn Host {
        self.host
    }

    /// The runtime directory, or why the host cannot name it.
    pub(super) fn run(&self) -> Result<&Path> {
        self.run.as_deref().map_err(Error::clone)
    }

    /// `error`, after the frozen QEMU of a launch that failed (`vm`, if it was spawned) has been killed and the files
    /// it could have made are removed. A QEMU that cannot be killed is named in the error: it is still there, frozen,
    /// and its files stay.
    fn abandon(&self, vm: Option<&mut VmProcess>, endpoint: &QmpEndpoint, claim: &FileClaim, error: Error) -> Error {
        if let Some(vm) = vm
            && let Err(kill) = vm.kill()
        {
            return Error::new(
                error.kind(),
                format!("{error}; the frozen QEMU could not be killed either ({kill}) and is still running"),
            );
        }
        // Only this launch's own files: nobody else draws its name while its claim file is there.
        remove_files(self.host, &endpoint.launch_files(), Some(claim), &mut |message| {
            self.progress.message(&message)
        });
        error
    }

    /// Everything of a launch after its names are reserved, up to the frozen process and its watch. `hold` is given to
    /// the process to inherit and dropped here, so the launch's own copy never outlives the spawn.
    fn start(
        &self,
        spec: &MachineSpec,
        install: &QemuInstall,
        endpoint: &QmpEndpoint,
        hold: Box<dyn Hold>,
        started: &mut Option<VmProcess>,
    ) -> Result<(MachineRecordParts, Arc<dyn crate::host::ProcessWatch>)> {
        let paths = LaunchPaths::resolve(spec, endpoint.path().to_path_buf(), &install.share)?;
        let args = build_args(spec, &paths)?;

        // An earlier run may have left files in the working directory; QEMU starts in an empty one.
        let cwd = spec.dir.join("qemu-cwd");
        remove_tree(&cwd).map_err(|e| Error::io(ErrorKind::Io, format!("cannot clear {}", cwd.display()), e))?;
        std::fs::create_dir_all(&cwd)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", cwd.display()), e))?;

        let mut command = cosca::Command::new();
        command
            .arg(install.tool(&format!("qemu-system-{}", qemu_arch(spec.arch))))
            .args(args)
            .env_clear()
            .envs(install.environment(self.host.system_dirs().as_ref())?)
            .current_dir(&cwd);
        // QEMU inherits the hold as descriptor 3 and keeps it, unused, until it exits or is killed.
        let inherited = hold
            .file()
            .try_clone()
            .map_err(|e| Error::io(ErrorKind::Io, "cannot duplicate the launch's hold", e))?;
        command
            .fd(INHERITED_HOLD, cosca::Stdio::from_file(inherited))
            .map_err(|e| Error::new(ErrorKind::Io, format!("cannot pass the launch's hold to QEMU: {e}")))?;
        let log = qemu_log(&spec.dir);
        let vm = started.insert(VmProcess::spawn_frozen(self.host, command, spec.lifetime, &log)?);
        drop(hold);
        let process = vm.record()?;
        let watch = self.host.watch_process(vm.id())?;
        Ok((MachineRecordParts { process, log }, watch))
    }
}

/// The descriptor the launch's hold has in QEMU. QEMU does not know of it.
const INHERITED_HOLD: i32 = 3;

/// What a successful `start` knows of the process.
struct MachineRecordParts {
    process: cosca::identity::ProcessIdRecord,
    log: PathBuf,
}

impl Backend for QemuBackend {
    /// Starts QEMU frozen, in an empty private working directory, with nothing in its environment but `SystemRoot` and
    /// a `PATH` of System32 and its own `bin`. Its output goes to `<instance>/qemu.log`. Resolving the sidecar here is
    /// the only place a launch can download it.
    ///
    /// The socket and claim file are reserved by `QmpEndpoint::reserve` and recorded in the machine's record. A host
    /// that cannot name the runtime directory fails the launch before anything is downloaded.
    fn launch(&self, spec: &MachineSpec) -> Result<Box<dyn Machine>> {
        let run = self.run()?;
        let install = self.install.get()?;
        let Reservation { endpoint, claim, hold } = QmpEndpoint::reserve(self.host, run, &*self.progress)?;
        let mut vm = None;
        match self.start(spec, &install, &endpoint, hold, &mut vm) {
            Ok((parts, watch)) => {
                let record = MachineRecord {
                    process: parts.process,
                    qmp: endpoint.path().display().to_string(),
                    ssh: Vec::new(),
                    serial_log: spec.serial_log.clone(),
                    leftovers: endpoint.launch_files().to_vec(),
                    claim: Some(claim.clone()),
                    no_reboot: spec.profile.no_reboot,
                };
                Ok(Box::new(QemuMachine::launched(
                    vm.expect("a started launch has its process"),
                    watch,
                    record,
                    claim,
                    spec.forwards.clone(),
                    parts.log,
                    spec.arch,
                    self.host,
                    self.progress.clone(),
                )))
            }
            Err(error) => Err(self.abandon(vm.as_mut(), &endpoint, &claim, error)),
        }
    }

    /// A record without a claim is refused: a session is only made against a machine's own files, which the claim says.
    fn attach(&self, record: &MachineRecord) -> Result<Box<dyn Machine>> {
        let Some(claim) = record.claim.clone() else {
            return Err(Error::new(
                ErrorKind::Internal,
                format!("the record of the machine at {} has no claim file", record.qmp),
            ));
        };
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
            claim,
            self.host,
            self.progress.clone(),
        )?))
    }

    fn storage(&self) -> &dyn Storage {
        &*self.storage
    }

    fn sweep(&self, warn: &mut dyn FnMut(String)) {
        self.sweep_run_dir(warn);
    }

    /// WHPX does not support saving a running machine's memory.
    fn can_save_memory(&self) -> bool {
        false
    }
}
