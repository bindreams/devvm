//! The fake backend: the real process machinery with a stand-in for QEMU. A "VM" is the `fake-vm` helper process,
//! started through `VmProcess::spawn_frozen`; its disk is a file naming the layer it sits on.
//!
//! Test code, so a missing helper fails the test visibly. Like QEMU's port-0 forward, the VM picks its own port and a
//! machine knows its endpoint only once it runs.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::backend::contract::ContractTarget;
use crate::backend::{
    Backend, Check, Detail, DeviceProfile, DiskBus, Endpoint, FileClaim, Machine, MachineExit, MachineRecord,
    MachineSpec, NetModel, Patch, SshEndpoint, Storage, Video, check_blank_disk_size, check_machine_spec,
    check_overlay, make_overlay,
};
use crate::contain::{Lifetime, VmProcess};
use crate::host::{self, Accel, Host};
use crate::instance::record::InstanceId;
use crate::store::layers::layers_tests::{key, request};
use crate::store::layers::{Built, ChainLease, LayerDir, LayerStore};
use crate::test_support::claimed::claim_file;
use crate::test_support::fake_vm;
use crate::{Arch, Error, ErrorKind, Network, Result};

#[cfg(test)]
#[path = "fake_tests.rs"]
mod fake_tests;

#[cfg(all(test, windows))]
#[path = "fake_windows_tests.rs"]
mod fake_windows_tests;

/// What a fake machine leaves outside its instance directory (see [`FakeBackend::with_leftovers_in`]).
struct Leftovers {
    dir: PathBuf,
    cleaned_by_kill: bool,
}

/// The files the `launch`th fake machine of instance `id` creates under `dir`: a socket and a session lock, then the
/// hold that is its claim (named as the QEMU backend names them). Every launch has names of its own, as a real one
/// draws them, so a machine never meets the files of an earlier one.
pub(crate) fn leftover_paths(dir: &Path, id: &InstanceId, launch: usize) -> Vec<PathBuf> {
    vec![
        dir.join(format!("{id}-{launch}.sock")),
        dir.join(format!("{id}-{launch}.sock.lock")),
        dir.join(format!("{id}-{launch}.sock.hold")),
    ]
}

/// The file in a layer that an instance disk sits on.
pub(crate) use crate::backend::LAYER_DISK;

pub(crate) struct FakeBackend {
    host: &'static dyn Host,
    /// Updated when a machine resumes and learns its endpoint.
    launched: Arc<Mutex<Vec<MachineRecord>>>,
    leftovers: Option<Leftovers>,
    specs: Mutex<Vec<MachineSpec>>,
    /// How many times a machine of this backend was reset.
    resets: Arc<AtomicUsize>,
    /// How many launches have named their files.
    named: AtomicUsize,
    storage: FakeStorage,
    /// What `checks` answers.
    checks: Vec<Check>,
    /// The detail of each `checks` call, in order.
    details_asked: Mutex<Vec<Detail>>,
    /// Where the machines report their SSH endpoint instead of the fake VM's own port.
    ssh_endpoint: Option<SocketAddr>,
    shutdown_fails: bool,
    launch_fails: bool,
    guest: Option<GuestScript>,
}

/// What the guest of a fake machine does once its machine runs: called by `resume` with the number of the launch (from
/// 0, in launch order through the backend), the spec the machine was launched with and the means to power the guest
/// off, which ends the VM process as a guest's shutdown ends QEMU. The script may write the machine's serial log and
/// its media, as a guest does, and may leave the machine running.
pub(crate) type GuestScript = Arc<dyn Fn(usize, &MachineSpec, &PowerOff) + Send + Sync>;

/// What a fake machine says of its VM process's exit: the fake VM prints nothing, so this stands in for QEMU's output.
pub(crate) const EXIT_OUTPUT: &str = "the fake VM's own account of its exit";

/// The power button of a fake machine's guest.
pub(crate) struct PowerOff {
    vm: SocketAddr,
}

impl PowerOff {
    /// Powers the guest off: the VM process exits with status 0. Fails the test if the VM cannot be asked.
    pub(crate) fn power_off(&self) {
        ask_to_exit(self.vm).unwrap_or_else(|e| panic!("the fake guest cannot power off: {e}"));
    }

    /// Ends the VM process with status 1, as QEMU dying does. Fails the test if the VM cannot be asked.
    pub(crate) fn crash(&self) {
        ask_to_crash(self.vm).unwrap_or_else(|e| panic!("the fake guest cannot crash: {e}"));
    }
}

/// Asks the fake VM at `addr` to exit, as a backend asks its guest to power off, and returns once it has closed the
/// connection.
pub(crate) fn ask_to_exit(addr: SocketAddr) -> Result<()> {
    ask(addr, fake_vm::SHUTDOWN_REQUEST)
}

/// Asks the fake VM at `addr` to exit with status 1, and returns once it has closed the connection.
pub(crate) fn ask_to_crash(addr: SocketAddr) -> Result<()> {
    ask(addr, fake_vm::CRASH_REQUEST)
}

fn ask(addr: SocketAddr, request: &str) -> Result<()> {
    let refused = |e| Error::io(ErrorKind::Io, format!("cannot ask the machine at {addr} to exit"), e);
    let mut stream = TcpStream::connect(addr).map_err(refused)?;
    // The VM greets first. Closing with its greeting unread would reset the connection and could drop the request.
    let mut greeting = String::new();
    BufReader::new(&stream).read_line(&mut greeting).map_err(refused)?;
    writeln!(stream, "{request}").map_err(refused)?;
    // The VM drops the connection when it exits, as a close or as a reset; the process exit is what is awaited.
    let _ = std::io::copy(&mut stream, &mut std::io::sink());
    Ok(())
}

impl FakeBackend {
    pub(crate) fn new(host: &'static dyn Host) -> FakeBackend {
        FakeBackend {
            host,
            launched: Arc::new(Mutex::new(Vec::new())),
            leftovers: None,
            specs: Mutex::new(Vec::new()),
            resets: Arc::new(AtomicUsize::new(0)),
            named: AtomicUsize::new(0),
            storage: FakeStorage::default(),
            checks: Vec::new(),
            details_asked: Mutex::new(Vec::new()),
            ssh_endpoint: None,
            shutdown_fails: false,
            launch_fails: false,
            guest: None,
        }
    }

    /// A backend whose machines have a guest that runs `script` when its machine runs.
    pub(crate) fn with_guest(mut self, script: GuestScript) -> FakeBackend {
        self.guest = Some(script);
        self
    }

    /// A backend whose machines refuse every graceful `shutdown`; `kill` still works.
    pub(crate) fn with_failing_shutdown(mut self) -> FakeBackend {
        self.shutdown_fails = true;
        self
    }

    /// A backend whose `launch` always fails.
    pub(crate) fn with_failing_launch(mut self) -> FakeBackend {
        self.launch_fails = true;
        self
    }

    /// A backend whose storage cannot commit a layer.
    pub(crate) fn with_failing_commit(mut self) -> FakeBackend {
        self.storage.commit_fails = true;
        self
    }

    /// A backend whose machines report `addr` as their `Endpoint::Normal`, the address of an SSH server a test runs,
    /// instead of the port of the fake VM process. The fake VM still listens on its own port, which `shutdown` asks.
    pub(crate) fn with_ssh_endpoint(mut self, addr: SocketAddr) -> FakeBackend {
        self.ssh_endpoint = Some(addr);
        self
    }

    /// A backend whose `checks` are `checks`.
    pub(crate) fn with_checks(mut self, checks: Vec<Check>) -> FakeBackend {
        self.checks = checks;
        self
    }

    /// The detail of each `checks` call so far, in order.
    pub(crate) fn details_asked(&self) -> Vec<Detail> {
        self.details_asked.lock().unwrap().clone()
    }

    /// How many resets the machines of this backend have taken.
    pub(crate) fn resets(&self) -> usize {
        self.resets.load(Ordering::SeqCst)
    }

    /// The count `resets` reads, for a guest that changes its boot id with each reset.
    pub(crate) fn reset_counter(&self) -> Arc<AtomicUsize> {
        self.resets.clone()
    }

    /// A backend whose machines create files outside their instance directory when they run, as QEMU's control socket
    /// and its lock file are, and name them in their record from the launch on. Under `dir`, one pair per instance.
    /// With `cleaned_by_kill` false, `kill` and `shutdown` leave the files, like a backend that does not clean up: the
    /// code around the machine must then.
    pub(crate) fn with_leftovers_in(mut self, dir: &Path, cleaned_by_kill: bool) -> FakeBackend {
        self.leftovers = Some(Leftovers {
            dir: dir.to_path_buf(),
            cleaned_by_kill,
        });
        self
    }

    /// How many machines were launched (spawned frozen) through this backend.
    pub(crate) fn launches(&self) -> usize {
        self.launched().len()
    }

    /// The record of every machine launched through this backend, in launch order.
    pub(crate) fn launched(&self) -> Vec<MachineRecord> {
        self.launched.lock().unwrap().clone()
    }

    /// The disk size every instance disk of this backend was asked for, in creation order.
    pub(crate) fn disk_sizes(&self) -> Vec<Option<u64>> {
        self.storage.sizes()
    }

    /// The spec of every machine launched through this backend, in launch order.
    pub(crate) fn specs(&self) -> Vec<MachineSpec> {
        self.specs.lock().unwrap().clone()
    }

    /// Makes the claim file `path`, which nothing holds afterwards: the fake VM is no process that could.
    fn claim(&self, path: &Path) -> Result<FileClaim> {
        claim_file(self.host, path).map_err(|e| Error::io(ErrorKind::Io, format!("cannot claim {}", path.display()), e))
    }

    fn cleaned_by_kill(&self) -> bool {
        self.leftovers
            .as_ref()
            .is_none_or(|leftovers| leftovers.cleaned_by_kill)
    }

    /// A disk a machine can run on: an instance disk whose layer is there, or a blank layer disk, which a build runs on.
    fn check_disk(disk: &Path) -> Result<()> {
        match std::fs::read_to_string(disk) {
            Ok(text) if text.starts_with("blank size=") => Ok(()),
            _ => FakeBackend::backing_layer(disk).map(drop),
        }
    }

    /// The layer file the instance disk `disk` sits on; an error when that file is gone.
    fn backing_layer(disk: &Path) -> Result<PathBuf> {
        let text = std::fs::read_to_string(disk)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot read the disk {}", disk.display()), e))?;
        let backing = text.strip_prefix("backing=").ok_or_else(|| {
            Error::new(
                ErrorKind::Store,
                format!("the disk {} is not a fake disk: {text:?}", disk.display()),
            )
        })?;
        let backing = PathBuf::from(backing);
        if backing.is_file() {
            Ok(backing)
        } else {
            Err(Error::new(
                ErrorKind::Store,
                format!(
                    "the disk {} sits on {}, which is gone (was its layer collected?)",
                    disk.display(),
                    backing.display()
                ),
            ))
        }
    }
}

impl Backend for FakeBackend {
    fn launch(&self, spec: &MachineSpec) -> Result<Box<dyn Machine>> {
        if self.launch_fails {
            return Err(Error::new(ErrorKind::Io, "the fake backend cannot launch"));
        }
        check_machine_spec(spec)?;
        FakeBackend::check_disk(&spec.disk)?;
        let state_dir = spec.serial_log.parent().expect("a serial log has a directory");
        fake_vm::forget_earlier_runs(state_dir).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot clear the earlier runs of {}", state_dir.display()),
                e,
            )
        })?;
        let vm = VmProcess::spawn_frozen(
            self.host,
            fake_vm::command(state_dir),
            spec.lifetime,
            &fake_vm::log(state_dir),
        )?;
        let leftovers = match &self.leftovers {
            Some(leftovers) => leftover_paths(&leftovers.dir, &spec.id, self.named.fetch_add(1, Ordering::SeqCst)),
            None => Vec::new(),
        };
        let claim = match leftovers.last() {
            Some(path) => Some(self.claim(path)?),
            None => None,
        };
        let record = MachineRecord {
            process: vm.record()?,
            qmp: String::new(),
            ssh: Vec::new(),
            serial_log: spec.serial_log.clone(),
            leftovers,
            claim,
            no_reboot: spec.profile.no_reboot,
        };
        let index = {
            let mut launched = self.launched.lock().unwrap();
            launched.push(record.clone());
            launched.len() - 1
        };
        self.specs.lock().unwrap().push(spec.clone());
        Ok(Box::new(FakeMachine {
            vm,
            record,
            ssh: Vec::new(),
            serial_log: spec.serial_log.clone(),
            state_dir: state_dir.to_path_buf(),
            launched: Some((self.launched.clone(), index)),
            cleaned_by_kill: self.cleaned_by_kill(),
            resets: self.resets.clone(),
            attached: false,
            host: self.host,
            ssh_endpoint: self.ssh_endpoint,
            shutdown_fails: self.shutdown_fails,
            guest: self.guest.clone().map(|script| (script, index, spec.clone())),
        }))
    }

    fn attach(&self, record: &MachineRecord) -> Result<Box<dyn Machine>> {
        let Some(vm) = VmProcess::attach(&record.process)? else {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("no machine is running as process {}", record.process.pid),
            ));
        };
        Ok(Box::new(FakeMachine {
            vm,
            record: record.clone(),
            ssh: record.ssh.iter().map(|e| (e.endpoint, e.addr)).collect(),
            serial_log: record.serial_log.clone(),
            state_dir: PathBuf::new(),
            launched: None,
            cleaned_by_kill: self.cleaned_by_kill(),
            resets: self.resets.clone(),
            attached: true,
            host: self.host,
            ssh_endpoint: self.ssh_endpoint,
            shutdown_fails: self.shutdown_fails,
            guest: None,
        }))
    }

    fn storage(&self) -> &dyn Storage {
        &self.storage
    }

    /// The fake's leftovers are deleted with its machine's record (`MachineRecord::remove_leftovers`), and nothing of
    /// it is shared between setups, so there is nothing to sweep.
    fn sweep(&self, _warn: &mut dyn FnMut(String)) {}

    fn checks(&self, detail: Detail) -> Vec<Check> {
        self.details_asked.lock().unwrap().push(detail);
        self.checks.clone()
    }

    fn can_save_memory(&self) -> bool {
        false
    }
}

struct FakeMachine {
    vm: VmProcess,
    record: MachineRecord,
    ssh: Vec<(Endpoint, SocketAddr)>,
    serial_log: PathBuf,
    /// Where the VM writes its `started` file.
    state_dir: PathBuf,
    /// The backend's list of launched records and this machine's place in it.
    launched: Option<(Arc<Mutex<Vec<MachineRecord>>>, usize)>,
    /// Whether `kill` and `shutdown` delete the record's `leftovers`.
    cleaned_by_kill: bool,
    resets: Arc<AtomicUsize>,
    /// Found with `attach`: already running, and not this process's to contain.
    attached: bool,
    host: &'static dyn Host,
    /// See [`FakeBackend::with_ssh_endpoint`].
    ssh_endpoint: Option<SocketAddr>,
    /// See [`FakeBackend::with_failing_shutdown`].
    shutdown_fails: bool,
    /// See [`FakeBackend::with_guest`]: the script, the launch's number and its spec.
    guest: Option<(GuestScript, usize, MachineSpec)>,
}

impl Machine for FakeMachine {
    fn record(&self) -> MachineRecord {
        self.record.clone()
    }

    fn ssh_endpoints(&self) -> &[(Endpoint, SocketAddr)] {
        &self.ssh
    }

    fn serial_log(&self) -> &Path {
        &self.serial_log
    }

    fn resume(&mut self) -> Result<()> {
        self.vm.resume()?;
        let own = SocketAddr::from((Ipv4Addr::LOCALHOST, fake_vm::await_port(&self.vm, &self.state_dir)));
        let addr = self.ssh_endpoint.unwrap_or(own);
        self.ssh = vec![(Endpoint::Normal, addr)];
        self.record.ssh = vec![SshEndpoint {
            endpoint: Endpoint::Normal,
            addr,
        }];
        if let Some((launched, index)) = &self.launched {
            launched.lock().unwrap()[*index] = self.record.clone();
        }
        // The running machine creates its files; the claim file was made at the launch.
        let claim = self.record.claim.as_ref().map(|claim| &claim.path);
        for path in self.record.leftovers.iter().filter(|path| Some(*path) != claim) {
            std::fs::write(path, b"")
                .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", path.display()), e))?;
        }
        if let Some((script, launch, spec)) = &self.guest {
            script(*launch, spec, &PowerOff { vm: own });
        }
        Ok(())
    }

    fn release(&mut self) -> Result<()> {
        if self.attached { Ok(()) } else { self.vm.release() }
    }

    fn try_wait(&self) -> Result<Option<MachineExit>> {
        Ok(self.vm.try_wait()?.map(|status| MachineExit {
            status,
            output: EXIT_OUTPUT.to_owned(),
        }))
    }

    /// Counts the reset: the fake guest's boot id changes when the code around it reads the count.
    fn reset(&mut self) -> Result<()> {
        if self.ssh.is_empty() {
            return Err(Error::new(
                ErrorKind::Internal,
                "a fake machine that has not run cannot be reset",
            ));
        }
        self.resets.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Asks the VM over its endpoint to exit, like a guest asked to power off, and waits for the process to be gone.
    fn shutdown(&mut self) -> Result<()> {
        if self.shutdown_fails {
            return Err(Error::new(
                ErrorKind::Io,
                "the fake machine's guest does not power down",
            ));
        }
        ask_to_exit(self.endpoint()?)?;
        self.vm.wait()?;
        self.remove_leftovers()
    }

    fn kill(&mut self) -> Result<()> {
        self.vm.kill()?;
        self.remove_leftovers()
    }
}

impl FakeMachine {
    /// Where the VM listens: the endpoint of the record, or, when that is an SSH server's (`with_ssh_endpoint`) or the
    /// record was taken before the machine ran, the port the VM reported in its state directory.
    fn endpoint(&self) -> Result<SocketAddr> {
        if self.ssh_endpoint.is_none()
            && let Some((_, addr)) = self.ssh.first()
        {
            return Ok(*addr);
        }
        let started = self
            .serial_log
            .parent()
            .expect("a serial log has a directory")
            .join(fake_vm::STARTED);
        match std::fs::read_to_string(&started) {
            Ok(port) => port
                .parse()
                .map(|port| SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
                .map_err(|e| {
                    Error::new(
                        ErrorKind::Internal,
                        format!("{} holds {port:?}: {e}", started.display()),
                    )
                }),
            Err(e) => Err(Error::io(
                ErrorKind::Internal,
                "a machine that has not run cannot be asked to shut down",
                e,
            )),
        }
    }

    /// Deletes the leftovers of a machine that is gone, if this backend's machines clean up after themselves.
    fn remove_leftovers(&self) -> Result<()> {
        if self.cleaned_by_kill {
            let mut failures = Vec::new();
            self.record.remove_leftovers(self.host, &mut |text| failures.push(text));
            if !failures.is_empty() {
                return Err(Error::new(ErrorKind::Io, failures.join("; ")));
            }
        }
        Ok(())
    }
}

/// Instance disks are files naming their layer; committing copies that name into the new layer. It ignores `size` and
/// records it.
#[derive(Default)]
pub(crate) struct FakeStorage {
    sizes: Mutex<Vec<Option<u64>>>,
    commit_fails: bool,
}

impl FakeStorage {
    /// The `size` of every `create_instance_disk`, in call order.
    pub(crate) fn sizes(&self) -> Vec<Option<u64>> {
        self.sizes.lock().unwrap().clone()
    }
}

impl Storage for FakeStorage {
    fn create_instance_disk(&self, layer: &LayerDir, into: &Path, size: Option<u64>) -> Result<()> {
        self.sizes.lock().unwrap().push(size);
        let text = format!("backing={}", layer.path().join(LAYER_DISK).display());
        std::fs::write(into, text)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot write the disk {}", into.display()), e))
    }

    fn import_base(&self, image: &Path, _arch: Arch, staging: &LayerDir) -> Result<()> {
        let target = staging.path().join(LAYER_DISK);
        std::fs::copy(image, &target).map(drop).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot copy {} to {}", image.display(), target.display()),
                e,
            )
        })
    }

    /// The disk file says it is blank and how large; there are no firmware variables, as for `import_base`.
    fn create_blank_disk(&self, size: u64, _arch: Arch, staging: &LayerDir) -> Result<()> {
        check_blank_disk_size(size)?;
        let target = staging.path().join(LAYER_DISK);
        std::fs::write(&target, format!("blank size={size}"))
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot write {}", target.display()), e))
    }

    /// A file naming the backing file and each patch's offset and length.
    fn overlay_with_patches(&self, backing: &Path, patches: &[Patch], into: &Path) -> Result<()> {
        check_overlay(backing, patches, into)?;
        let patches: Vec<String> = patches
            .iter()
            .map(|patch| format!("{}+{}", patch.offset, patch.bytes.len()))
            .collect();
        make_overlay(into, |staging, _| {
            std::fs::write(
                staging,
                format!("overlay of {} patches={}", backing.display(), patches.join(",")),
            )
            .map_err(|e| {
                Error::io(
                    ErrorKind::Io,
                    format!("cannot write the overlay {}", staging.display()),
                    e,
                )
            })
        })
    }

    fn commit_layer(&self, instance_disk: &Path, staging: &LayerDir) -> Result<()> {
        if self.commit_fails {
            return Err(Error::new(ErrorKind::Io, "the fake storage cannot commit"));
        }
        let disk = std::fs::read_to_string(instance_disk).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot read the disk {}", instance_disk.display()),
                e,
            )
        })?;
        let target = staging.path().join(LAYER_DISK);
        std::fs::write(&target, format!("committed from {disk}"))
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot write {}", target.display()), e))
    }
}

/// A spec for a fake machine whose disk and serial log are at the given paths.
pub(crate) fn fake_spec(disk: PathBuf, serial_log: PathBuf, lifetime: Lifetime) -> MachineSpec {
    MachineSpec {
        id: InstanceId::random(),
        dir: disk.parent().expect("a disk has a directory").to_path_buf(),
        arch: Arch::Amd64,
        accel: Accel::Tcg,
        cpus: 1,
        memory: 1 << 30,
        network: Network::Nat,
        disk,
        media: Vec::new(),
        profile: DeviceProfile {
            disk: DiskBus::Virtio,
            net: NetModel::Virtio,
            video: Video::None,
            no_reboot: false,
        },
        firmware: None,
        forwards: vec![Endpoint::Normal],
        serial_log,
        lifetime,
    }
}

/// The fake backend as a contract target.
pub(crate) struct FakeTarget {
    backend: Arc<FakeBackend>,
    /// Where the machines of this target leave their files; they clean up after themselves.
    run_dir: tempfile::TempDir,
}

impl FakeTarget {
    pub(crate) fn new() -> FakeTarget {
        let run_dir = tempfile::tempdir().expect("a temporary directory");
        FakeTarget {
            backend: Arc::new(FakeBackend::new(host::current()).with_leftovers_in(run_dir.path(), true)),
            run_dir,
        }
    }
}

impl FakeTarget {
    pub(crate) fn launches(&self) -> usize {
        self.backend.launches()
    }
}

impl ContractTarget for FakeTarget {
    fn backend(&self) -> Arc<dyn Backend> {
        self.backend.clone()
    }

    fn parent_layer(&self, store: &LayerStore) -> ChainLease {
        store
            .build(&request(key(1)), None, |staging| {
                std::fs::write(staging.dir.path().join(LAYER_DISK), b"base disk").unwrap();
                Ok(Built::default())
            })
            .unwrap()
    }

    fn spec(&self, root: &Path, n: u32) -> MachineSpec {
        let dir = root.join(format!("machine-{n}"));
        fake_spec(dir.join("disk.img"), dir.join("serial.log"), Lifetime::OutlivesOwner)
    }

    fn shutdown_elsewhere(&self, record: &MachineRecord) -> Result<()> {
        let output = crate::test_support::helper::command("fake-shutdown", &[&serde_json::to_string(record).unwrap()])
            .output()
            .expect("the shutdown helper should run");
        if output.status.success() {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::Internal,
                format!(
                    "the shutdown helper failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ),
            ))
        }
    }

    fn ended_gracefully(&self, spec: &MachineSpec) -> bool {
        spec.serial_log
            .parent()
            .expect("a serial log has a directory")
            .join(fake_vm::GRACEFUL)
            .exists()
    }

    fn has_run(&self, spec: &MachineSpec) -> bool {
        spec.serial_log
            .parent()
            .expect("a serial log has a directory")
            .join(fake_vm::STARTED)
            .exists()
    }

    fn leftover_files(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.run_dir.path())
            .expect("the run directory exists")
            .map(|entry| entry.expect("a directory entry").path())
            .collect()
    }

    fn launch_elsewhere(&self, spec: &MachineSpec) -> Result<MachineRecord> {
        let output = crate::test_support::helper::command(
            "fake-launcher",
            &[
                &spec.disk.display().to_string(),
                &spec.serial_log.display().to_string(),
                &spec.id.to_string(),
                &self.run_dir.path().display().to_string(),
            ],
        )
        .output()
        .expect("the launcher should run");
        if !output.status.success() {
            return Err(Error::new(
                ErrorKind::Internal,
                format!("the launcher failed: {}", String::from_utf8_lossy(&output.stderr)),
            ));
        }
        serde_json::from_slice(&output.stdout).map_err(|e| {
            Error::new(
                ErrorKind::Internal,
                format!(
                    "the launcher printed no machine record ({e}): {}",
                    String::from_utf8_lossy(&output.stdout)
                ),
            )
        })
    }
}

/// Helper `fake-shutdown <machine-record-json>`: attaches to the machine, waits for it to be booted and shuts it down.
pub(crate) fn fake_shutdown(args: &[String]) {
    let record: MachineRecord = serde_json::from_str(&args[0]).expect("the argument is a machine record");
    let mut machine = FakeBackend::new(host::current())
        .attach(&record)
        .expect("the machine should be attachable");
    FakeTarget::new()
        .await_booted(&*machine)
        .expect("the machine should be booted");
    machine.shutdown().expect("the machine should shut down");
}

/// Helper `fake-launcher <disk> <serial-log> <instance-id> <leftover-dir>`: launches an `OutlivesOwner` machine, resumes and releases it, prints
/// its record and exits, leaving the machine running.
pub(crate) fn fake_launcher(args: &[String]) {
    let backend = FakeBackend::new(host::current()).with_leftovers_in(Path::new(&args[3]), true);
    let mut spec = fake_spec(
        PathBuf::from(&args[0]),
        PathBuf::from(&args[1]),
        Lifetime::OutlivesOwner,
    );
    spec.id = args[2].parse().expect("the instance id");
    let mut machine = backend.launch(&spec).expect("the machine should launch");
    machine.resume().expect("the machine should resume");
    machine.release().expect("the machine should release");
    println!("{}", serde_json::to_string(&machine.record()).unwrap());
}
