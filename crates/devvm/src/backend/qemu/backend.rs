//! The QEMU backend: launches QEMU frozen from a `MachineSpec` and finds it again from a record.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::args::{ArchFiles, LaunchPaths, build_args};
use super::checks::qemu_check;
use super::machine::{QemuMachine, qemu_log};
use super::qmp::{QmpEndpoint, RUN_LOCK, Reservation};
use super::sidecar::{LazyInstall, QemuInstall};
use super::storage::QemuStorage;
use crate::backend::{Backend, Check, Detail, FileClaim, Machine, MachineRecord, MachineSpec, Storage, remove_files};
use crate::contain::{AttachOutcome, VmCommand, VmProcess};
use crate::host::{Hold, HoldOutcome, Host, is_absent, remove_tree};
use crate::progress::Progress;
use crate::store::lock::{FileLock, Mode};
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "backend_tests.rs"]
mod backend_tests;

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
            self.progress.warn(&message)
        });
        error
    }

    /// What `attach` says of a machine whose process record this devvm cannot read, which says nothing of whether its
    /// process runs. The launch's claim does: QEMU holds it for as long as it runs, so a claim that can be taken is a
    /// machine that is over (`NotFound`, as for a dead process), the proof `remove_files` and the sweep rely on. A claim
    /// that is held is a machine that runs, which this devvm cannot manage: an error that names the process to end.
    ///
    /// The runtime directory's lock is held shared while the claim is looked at, as `reserve` holds it: a sweep's own
    /// brief hold on a dead machine's claim is then never read as a machine that runs.
    fn without_readable_record(
        &self,
        record: &MachineRecord,
        claim: &FileClaim,
        unreadable: cosca::error::Error,
    ) -> Error {
        let pid = record.process.pid;
        match self.claim_is_free(claim) {
            Ok(true) => not_running(record),
            Ok(false) => Error::new(
                ErrorKind::Io,
                format!(
                    "the VM runs as process {pid}, and this devvm cannot read the VM's process record: {unreadable};                      end process {pid} and run the command again"
                ),
            )
            .with_source(unreadable),
            Err(e) => e.with_context(
                ErrorKind::Io,
                format!("cannot tell whether the VM process {pid} is running: {unreadable}"),
            ),
        }
    }

    /// Whether nobody holds `claim`'s file: no QEMU of the launch runs, whether the file is there or not.
    fn claim_is_free(&self, claim: &FileClaim) -> Result<bool> {
        let Some(run) = claim.path.parent() else {
            return Err(Error::new(
                ErrorKind::Internal,
                format!("the claim file {} is in no directory", claim.path.display()),
            ));
        };
        // A runtime directory with no lock was never launched in.
        let mut directory = FileLock::open_existing(&run.join(RUN_LOCK))?;
        if let Some(directory) = &mut directory {
            directory.lock(Mode::Shared, "a sweep of the runtime directory", &mut |m| {
                self.progress.message(m)
            })?;
        }
        match self.host.hold_existing(&claim.path) {
            Ok(HoldOutcome::Taken(_) | HoldOutcome::Absent) => Ok(true),
            Ok(HoldOutcome::Busy) => Ok(false),
            Err(e) => Err(Error::io(
                ErrorKind::Io,
                format!("cannot hold {}", claim.path.display()),
                e,
            )),
        }
    }

    /// Everything of a launch after its names are reserved, up to the frozen process and its watch. `hold` is shared
    /// with the frozen process and dropped here, so the launch's own copy never outlives the spawn.
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

        let tool = install.tool(&ArchFiles::of(spec.arch).system);
        let command = VmCommand {
            args,
            clear_env: true,
            env: install.environment(self.host.system_dirs().as_ref())?,
            cwd: Some(cwd),
        };
        // Every launch in a directory appends to its `qemu.log`: this one's output begins where the file ends now.
        let log = qemu_log(&spec.dir);
        let log_from = log_length(&log)?;
        let vm = started.insert(VmProcess::spawn_frozen(self.host, &tool, command, spec.lifetime, &log)?);
        // QEMU keeps the hold, unused, until it exits or is killed.
        hold.share_with(vm.id())
            .map_err(|e| Error::io(ErrorKind::Io, "cannot give the launch's hold to QEMU", e))?;
        drop(hold);
        let process = vm.record()?;
        let watch = self.host.watch_process(vm.id())?;
        Ok((MachineRecordParts { process, log, log_from }, watch))
    }
}

/// `NotFound`: the machine `record` names is not running.
fn not_running(record: &MachineRecord) -> Error {
    Error::new(
        ErrorKind::NotFound,
        format!("no machine is running as process {}", record.process.pid),
    )
}

/// How long the file `log` is, 0 when it is not there. A link is followed: QEMU appends to the file it leads to.
fn log_length(log: &Path) -> Result<u64> {
    match std::fs::metadata(log) {
        Ok(meta) => Ok(meta.len()),
        Err(e) if is_absent(&e) => Ok(0),
        Err(e) => Err(Error::io(
            ErrorKind::Io,
            format!("cannot look at the VM log {}", log.display()),
            e,
        )),
    }
}

/// What a successful `start` knows of the process.
struct MachineRecordParts {
    process: cosca::identity::ProcessIdRecord,
    log: PathBuf,
    /// Where this launch's output begins in `log`.
    log_from: u64,
}

impl Backend for QemuBackend {
    /// Starts QEMU frozen, in an empty private working directory, with nothing in its environment but `SystemRoot` and
    /// a `PATH` of System32 and its own `bin`. Its output goes to `<instance>/qemu.log`. Resolving the sidecar here is
    /// the only place a launch can download it.
    ///
    /// The socket and claim file are reserved by `QmpEndpoint::reserve` and recorded in the machine's record. A host
    /// that cannot name or use the runtime directory fails the launch before anything is downloaded.
    fn launch(&self, spec: &MachineSpec) -> Result<Box<dyn Machine>> {
        let run = self.run()?;
        QmpEndpoint::prepare_run_dir(self.host, run)?;
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
                    parts.log_from,
                    spec.arch,
                    self.host,
                    self.progress.clone(),
                )))
            }
            Err(error) => Err(self.abandon(vm.as_mut(), &endpoint, &claim, error)),
        }
    }

    /// A record without a claim is refused: a session is only made against a machine's own files, which the claim says.
    /// A record whose process record cannot be read is decided by the claim (`without_readable_record`).
    fn attach(&self, record: &MachineRecord) -> Result<Box<dyn Machine>> {
        let Some(claim) = record.claim.clone() else {
            return Err(Error::new(
                ErrorKind::Internal,
                format!("the record of the machine at {} has no claim file", record.qmp),
            ));
        };
        let vm = match VmProcess::attach(&record.process)? {
            AttachOutcome::Running(vm) => vm,
            AttachOutcome::NotRunning => return Err(not_running(record)),
            AttachOutcome::Unreadable(unreadable) => {
                return Err(self.without_readable_record(record, &claim, unreadable));
            }
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

    fn checks(&self, detail: Detail) -> Vec<Check> {
        vec![qemu_check(&self.install, detail)]
    }

    /// WHPX does not support saving a running machine's memory.
    fn can_save_memory(&self) -> bool {
        false
    }
}
