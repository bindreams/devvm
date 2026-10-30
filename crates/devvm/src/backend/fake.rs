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
    Backend, DeviceProfile, DiskBus, Endpoint, Machine, MachineRecord, MachineSpec, NetModel, SshEndpoint, Storage,
};
use crate::contain::{Lifetime, VmProcess};
use crate::host::{self, Accel, Host};
use crate::instance::record::InstanceId;
use crate::store::layers::layers_tests::{key, request};
use crate::store::layers::{Built, ChainLease, LayerDir, LayerStore};
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

/// The files a fake machine of instance `id` creates under `dir`.
pub(crate) fn leftover_paths(dir: &Path, id: &InstanceId) -> Vec<PathBuf> {
    vec![dir.join(format!("{id}.sock")), dir.join(format!("{id}.sock.lock"))]
}

/// The file in a layer that an instance disk sits on.
const LAYER_DISK: &str = "disk.qcow2";

pub(crate) struct FakeBackend {
    host: &'static dyn Host,
    /// Updated when a machine resumes and learns its endpoint.
    launched: Arc<Mutex<Vec<MachineRecord>>>,
    leftovers: Option<Leftovers>,
    specs: Mutex<Vec<MachineSpec>>,
    /// How many times a machine of this backend was reset.
    resets: Arc<AtomicUsize>,
    storage: FakeStorage,
}

impl FakeBackend {
    pub(crate) fn new(host: &'static dyn Host) -> FakeBackend {
        FakeBackend {
            host,
            launched: Arc::new(Mutex::new(Vec::new())),
            leftovers: None,
            specs: Mutex::new(Vec::new()),
            resets: Arc::new(AtomicUsize::new(0)),
            storage: FakeStorage,
        }
    }

    /// How many resets the machines of this backend have taken.
    pub(crate) fn resets(&self) -> usize {
        self.resets.load(Ordering::SeqCst)
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

    /// The spec of every machine launched through this backend, in launch order.
    pub(crate) fn specs(&self) -> Vec<MachineSpec> {
        self.specs.lock().unwrap().clone()
    }

    fn cleaned_by_kill(&self) -> bool {
        self.leftovers
            .as_ref()
            .is_none_or(|leftovers| leftovers.cleaned_by_kill)
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
        FakeBackend::backing_layer(&spec.disk)?;
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
        let record = MachineRecord {
            process: vm.record()?,
            qmp: String::new(),
            ssh: Vec::new(),
            serial_log: spec.serial_log.clone(),
            leftovers: self
                .leftovers
                .as_ref()
                .map(|leftovers| leftover_paths(&leftovers.dir, &spec.id))
                .unwrap_or_default(),
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
        }))
    }

    fn storage(&self) -> &dyn Storage {
        &self.storage
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
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, fake_vm::await_port(&self.vm, &self.state_dir)));
        self.ssh = vec![(Endpoint::Normal, addr)];
        self.record.ssh = vec![SshEndpoint {
            endpoint: Endpoint::Normal,
            addr,
        }];
        if let Some((launched, index)) = &self.launched {
            launched.lock().unwrap()[*index] = self.record.clone();
        }
        // The running machine creates its files.
        for path in &self.record.leftovers {
            std::fs::write(path, b"")
                .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", path.display()), e))?;
        }
        Ok(())
    }

    fn release(&mut self) -> Result<()> {
        if self.attached { Ok(()) } else { self.vm.release() }
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
        let addr = self.endpoint()?;
        let ask = |e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot ask the machine at {addr} to shut down"),
                e,
            )
        };
        let mut stream = TcpStream::connect(addr).map_err(ask)?;
        // The VM greets first. Closing with its greeting unread would reset the connection and could drop the request.
        let mut greeting = String::new();
        BufReader::new(&stream).read_line(&mut greeting).map_err(ask)?;
        writeln!(stream, "{}", fake_vm::SHUTDOWN_REQUEST).map_err(ask)?;
        // The VM drops the connection when it exits, as a close or as a reset; the process exit is what is awaited.
        let _ = std::io::copy(&mut stream, &mut std::io::sink());
        self.vm.wait()?;
        self.remove_leftovers()
    }

    fn kill(&mut self) -> Result<()> {
        self.vm.kill()?;
        self.remove_leftovers()
    }
}

impl FakeMachine {
    /// Where the VM listens: the endpoint of the record, or, for a record taken before the machine ran, the port the VM
    /// reported in its state directory.
    fn endpoint(&self) -> Result<SocketAddr> {
        if let Some((_, addr)) = self.ssh.first() {
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
            self.record.remove_leftovers(&mut |text| failures.push(text));
            if !failures.is_empty() {
                return Err(Error::new(ErrorKind::Io, failures.join("; ")));
            }
        }
        Ok(())
    }
}

/// Instance disks are files naming their layer; committing copies that name into the new layer.
pub(crate) struct FakeStorage;

impl Storage for FakeStorage {
    fn create_instance_disk(&self, layer: &LayerDir, into: &Path, _size: u64) -> Result<()> {
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

    fn commit_layer(&self, instance_disk: &Path, staging: &LayerDir) -> Result<()> {
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

    fn leftovers(&self, spec: &MachineSpec) -> Vec<PathBuf> {
        leftover_paths(self.run_dir.path(), &spec.id)
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
