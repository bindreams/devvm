//! The fake backend: the real process machinery with a stand-in for QEMU. A "VM" is the `fake-vm` helper process,
//! started through `VmProcess::spawn_frozen`; its disk is a file naming the layer it sits on.
//!
//! Test code, so a lost port race or a missing helper fails the test visibly.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::backend::contract::ContractTarget;
use crate::backend::{
    Backend, DeviceProfile, DiskBus, Endpoint, Machine, MachineRecord, MachineSpec, NetModel, SshEndpoint, Storage,
};
use crate::contain::{Lifetime, VmProcess};
use crate::host::{self, Accel, Host};
use crate::store::layers::layers_tests::{key, request};
use crate::store::layers::{Built, ChainLease, LayerDir, LayerStore};
use crate::test_support::fake_vm;
use crate::{Arch, Error, ErrorKind, Network, Result};
use std::sync::Arc;

#[cfg(test)]
#[path = "fake_tests.rs"]
mod fake_tests;

#[cfg(all(test, windows))]
#[path = "fake_windows_tests.rs"]
mod fake_windows_tests;

/// The file in a layer that an instance disk sits on.
const LAYER_DISK: &str = "disk.qcow2";

pub(crate) struct FakeBackend {
    host: &'static dyn Host,
    launches: AtomicUsize,
    ports: Mutex<HashSet<u16>>,
    storage: FakeStorage,
}

impl FakeBackend {
    pub(crate) fn new(host: &'static dyn Host) -> FakeBackend {
        FakeBackend {
            host,
            launches: AtomicUsize::new(0),
            ports: Mutex::new(HashSet::new()),
            storage: FakeStorage,
        }
    }

    /// How many machines were launched (spawned frozen) through this backend.
    pub(crate) fn launches(&self) -> usize {
        self.launches.load(Ordering::SeqCst)
    }

    /// A loopback port no machine of this backend has had.
    fn claim_port(&self) -> u16 {
        loop {
            let port = fake_vm::free_port();
            if self.ports.lock().unwrap().insert(port) {
                return port;
            }
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
        FakeBackend::backing_layer(&spec.disk)?;
        let state_dir = spec.serial_log.parent().expect("a serial log has a directory");
        let port = self.claim_port();
        let vm = VmProcess::spawn_frozen(self.host, fake_vm::command(port, state_dir), spec.lifetime)?;
        self.launches.fetch_add(1, Ordering::SeqCst);
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let record = MachineRecord {
            process: vm.record()?,
            qmp: String::new(),
            ssh: vec![SshEndpoint {
                endpoint: Endpoint::Normal,
                addr,
            }],
        };
        Ok(Box::new(FakeMachine {
            vm,
            record,
            ssh: vec![(Endpoint::Normal, addr)],
            serial_log: spec.serial_log.clone(),
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
            serial_log: PathBuf::new(),
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
        self.vm.resume()
    }

    fn release(&mut self) -> Result<()> {
        if self.attached { Ok(()) } else { self.vm.release() }
    }

    /// A fake has no guest to ask, so this is a kill.
    fn shutdown(&mut self) -> Result<()> {
        self.vm.kill()
    }

    fn kill(&mut self) -> Result<()> {
        self.vm.kill()
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
}

impl FakeTarget {
    pub(crate) fn new() -> FakeTarget {
        FakeTarget {
            backend: Arc::new(FakeBackend::new(host::current())),
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

    fn launch_elsewhere(&self, spec: &MachineSpec) -> Result<MachineRecord> {
        let output = crate::test_support::helper::command(
            "fake-launcher",
            &[&spec.disk.display().to_string(), &spec.serial_log.display().to_string()],
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

/// Helper `fake-launcher <disk> <serial-log>`: launches an `OutlivesOwner` machine, resumes and releases it, prints
/// its record and exits, leaving the machine running.
pub(crate) fn fake_launcher(args: &[String]) {
    let backend = FakeBackend::new(host::current());
    let spec = fake_spec(
        PathBuf::from(&args[0]),
        PathBuf::from(&args[1]),
        Lifetime::OutlivesOwner,
    );
    let mut machine = backend.launch(&spec).expect("the machine should launch");
    machine.resume().expect("the machine should resume");
    machine.release().expect("the machine should release");
    println!("{}", serde_json::to_string(&machine.record()).unwrap());
}
