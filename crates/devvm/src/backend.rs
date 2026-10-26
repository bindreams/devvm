//! The VM backend: how a machine is launched, found again from another process, and given its disks. QEMU is one
//! implementation; the fake backend in the tests is another.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use cosca::identity::ProcessIdRecord;
use serde::{Deserialize, Serialize};

use crate::contain::Lifetime;
use crate::host::Accel;
use crate::instance::record::InstanceId;
use crate::store::layers::LayerDir;
use crate::{Arch, Network, Result};

#[cfg(test)]
#[path = "backend_tests.rs"]
mod backend_tests;

#[cfg(test)]
pub(crate) mod contract;
#[cfg(test)]
pub(crate) mod fake;

pub(crate) trait Backend: Send + Sync {
    /// Starts a machine and returns it **frozen**: its identity is known (`Machine::record`) but it has not run. The
    /// caller persists the record, then calls `Machine::resume`.
    fn launch(&self, spec: &MachineSpec) -> Result<Box<dyn Machine>>;

    /// Finds a machine started by another process (or an earlier run) from its record.
    fn attach(&self, rec: &MachineRecord) -> Result<Box<dyn Machine>>;

    fn storage(&self) -> &dyn Storage;

    /// Whether this backend can save a running machine's memory.
    fn can_save_memory(&self) -> bool;
}

pub(crate) trait Storage: Send + Sync {
    /// Makes `into` a copy-on-write disk of `size` bytes on top of `layer`.
    fn create_instance_disk(&self, layer: &LayerDir, into: &Path, size: u64) -> Result<()>;

    /// Turns an instance's disk into a layer. Writes only into `staging` and never modifies a parent layer.
    fn commit_layer(&self, instance_disk: &Path, staging: &LayerDir) -> Result<()>;
}

pub(crate) trait Machine: Send {
    fn record(&self) -> MachineRecord;
    fn ssh_endpoints(&self) -> &[(Endpoint, SocketAddr)];
    fn serial_log(&self) -> &Path;

    /// Lets a frozen machine run. `record()` is valid before it. `Internal` on a machine from `Backend::attach`.
    fn resume(&mut self) -> Result<()>;

    /// Disarms the containment of a resumed machine: it now outlives this process. `Ok` on a machine from
    /// `Backend::attach`.
    fn release(&mut self) -> Result<()>;

    /// Graceful shutdown; returns after the process has exited.
    fn shutdown(&mut self) -> Result<()>;

    /// Kills the machine; returns after the process has exited. A frozen machine never runs.
    fn kill(&mut self) -> Result<()>;
}

/// Which guest port a forward reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Endpoint {
    Normal,
    Elevated,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SshEndpoint {
    pub endpoint: Endpoint,
    pub addr: SocketAddr,
}

/// What an instance record keeps about its machine so another process can find it again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MachineRecord {
    pub process: ProcessIdRecord,
    /// Where the machine's control channel listens.
    pub qmp: String,
    pub ssh: Vec<SshEndpoint>,
    /// The machine's serial console log, named in boot-failure errors by a machine found with `Backend::attach`.
    pub serial_log: PathBuf,
    /// Files outside the instance directory that die with the machine (QEMU: the QMP socket and its lock file).
    #[serde(default)]
    pub leftovers: Vec<PathBuf>,
}

impl MachineRecord {
    /// Deletes `leftovers`, best effort: a file that is already gone is fine, any other failure goes to `warn`.
    /// Call it only once the machine is proven dead.
    pub(crate) fn remove_leftovers(&self, warn: &mut dyn FnMut(String)) {
        for path in &self.leftovers {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn(format!("cannot delete {}: {e}", path.display())),
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MachineSpec {
    /// The instance this machine belongs to; a backend derives names that must not collide from it.
    pub id: InstanceId,
    /// The instance directory. `disk`, `serial_log` and the firmware variables sit in it.
    pub dir: PathBuf,
    pub arch: Arch,
    pub accel: Accel,
    pub cpus: u32,
    pub memory: u64,
    pub network: Network,
    pub disk: PathBuf,
    pub media: Vec<Media>,
    pub profile: DeviceProfile,
    pub firmware: Option<Firmware>,
    pub forwards: Vec<Endpoint>,
    pub serial_log: PathBuf,
    pub lifetime: Lifetime,
}

#[derive(Clone, Debug)]
pub(crate) struct Media {
    pub path: PathBuf,
    pub kind: MediaKind,
    pub boot_index: Option<u32>,
    pub readonly: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaKind {
    Cdrom,
    Disk,
}

#[derive(Clone, Debug)]
pub(crate) struct Firmware {
    pub code: PathBuf,
    pub vars: Option<PathBuf>,
}

/// The device models a guest needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeviceProfile {
    pub disk: DiskBus,
    pub net: NetModel,
    /// Do not reboot after a guest-initiated reset (installs).
    pub no_reboot: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiskBus {
    Virtio,
    Sata,
    Nvme,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NetModel {
    Virtio,
    E1000,
}
