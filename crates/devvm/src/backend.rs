//! The VM backend: how a machine is launched, found again from another process, and given its disks. QEMU is one
//! implementation; the fake backend in the tests is another.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use cosca::identity::ProcessIdRecord;
use serde::{Deserialize, Serialize};

use crate::contain::Lifetime;
use crate::host::{Accel, Comparison, FileId, Hold, HoldOutcome, Host, is_absent, symlink_metadata_if_present};
use crate::instance::record::InstanceId;
use crate::store::layers::LayerDir;
use crate::{Arch, ErrorKind, Network, Result};

#[cfg(test)]
#[path = "backend_tests.rs"]
mod backend_tests;

#[cfg(test)]
pub(crate) mod contract;
#[cfg(test)]
pub(crate) mod fake;
pub(crate) mod qemu;

pub(crate) trait Backend: Send + Sync {
    /// Starts a machine and returns it **frozen**: its identity is known (`Machine::record`) but it has not run. The
    /// caller persists the record, then calls `Machine::resume`. The caller holds the instance's lock and no machine of
    /// the instance runs: a backend may delete what an earlier machine of the instance left behind.
    fn launch(&self, spec: &MachineSpec) -> Result<Box<dyn Machine>>;

    /// Finds a machine started by another process (or an earlier run) from its record. `NotFound` when it is not
    /// running. This is how every process finds a machine again, whatever a machine is in this backend.
    fn attach(&self, rec: &MachineRecord) -> Result<Box<dyn Machine>>;

    /// `attach`, with `None` for a machine that is not running.
    fn find(&self, rec: &MachineRecord) -> Result<Option<Box<dyn Machine>>> {
        match self.attach(rec) {
            Ok(machine) => Ok(Some(machine)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn storage(&self) -> &dyn Storage;

    /// Deletes the files that machines left outside their instance directories and that no machine of any devvm setup
    /// uses any more. A setup other than the caller's may be running machines, so only a file that nothing can be using
    /// goes. Best effort: a problem is told to `warn` and left for the next sweep. A backend whose machines leave no
    /// such files does nothing.
    fn sweep(&self, warn: &mut dyn FnMut(String));

    /// Whether this backend can save a running machine's memory.
    fn can_save_memory(&self) -> bool;
}

/// The instance disk size when the recipe sets none and the layer's disk is no bigger.
pub(crate) const DEFAULT_DISK: u64 = 40 << 30;

pub(crate) trait Storage: Send + Sync {
    /// Makes `into` a copy-on-write disk on top of `layer`. `Some(size)` smaller than the layer's disk (after the
    /// backend's size rounding), or one the backend cannot make, is `Recipe`, with nothing made. `None` is
    /// `DEFAULT_DISK` or the layer's disk, whichever is larger. A layer the backend cannot cover is `Store`.
    fn create_instance_disk(&self, layer: &LayerDir, into: &Path, size: Option<u64>) -> Result<()>;

    /// Turns an instance's disk into a layer. Writes only into `staging` and never modifies a parent layer.
    fn commit_layer(&self, instance_disk: &Path, staging: &LayerDir) -> Result<()>;

    /// Makes the downloaded disk `image` (any format the backend reads) the root layer's disk in `staging`, with the
    /// firmware variables template for `arch`. Writes only into `staging`.
    fn import_base(&self, image: &Path, arch: Arch, staging: &LayerDir) -> Result<()>;
}

pub(crate) trait Machine: Send {
    fn record(&self) -> MachineRecord;
    fn ssh_endpoints(&self) -> &[(Endpoint, SocketAddr)];
    fn serial_log(&self) -> &Path;

    /// Lets a frozen machine run and returns once its endpoints are known: `ssh_endpoints()` and the `ssh` of
    /// `record()` are valid only from then on. `record()` is valid before, apart from the endpoints. `Internal` on a
    /// machine from `Backend::attach`.
    fn resume(&mut self) -> Result<()>;

    /// Disarms the containment of a resumed machine: it now outlives this process. `Ok` on a machine from
    /// `Backend::attach`.
    fn release(&mut self) -> Result<()>;

    /// Resets the running guest inside its VM process and returns once the reset has happened, not when the guest is
    /// up again. The process, the disk and the forwarded ports are untouched. `Unavailable` when this host or the VM's
    /// state cannot reset it (nothing is sent then); `NotSupported` for a machine that has no reset.
    fn reset(&mut self) -> Result<()> {
        Err(crate::Error::new(
            ErrorKind::NotSupported,
            "this machine cannot be reset in place",
        ))
    }

    /// Graceful shutdown; returns after the process has exited and its `leftovers` are deleted.
    fn shutdown(&mut self) -> Result<()>;

    /// Kills the machine; returns after the process has exited and its `leftovers` are deleted. A frozen machine never
    /// runs.
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
    /// Files outside the instance directory that die with the machine (QEMU: the QMP socket, the lock file of its
    /// sessions and the hold, in that order: the hold last, since it keeps the name reserved while the others go).
    #[serde(default)]
    pub leftovers: Vec<PathBuf>,
    /// The file among `leftovers` that this launch made and that is held while the machine runs. `None` in a record
    /// of a machine that has no such file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<FileClaim>,
    /// The machine was launched with `DeviceProfile::no_reboot`: it exits when its guest reboots, so it cannot be reset
    /// in place.
    #[serde(default)]
    pub no_reboot: bool,
}

/// The file a launch made to reserve its names: where it is and which file it was when made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileClaim {
    pub path: PathBuf,
    pub id: FileId,
}

impl MachineRecord {
    /// Deletes `leftovers` (see [`remove_files`]). Call it only once the machine is proven dead.
    pub(crate) fn remove_leftovers(&self, host: &dyn Host, warn: &mut dyn FnMut(String)) {
        remove_files(host, &self.leftovers, self.claim.as_ref(), warn);
    }
}

/// Deletes `paths`, the files of a dead machine, best effort. They go only while the caller holds `claim`'s file, the
/// one the launch made, and it is still that file: a machine that is alive holds it, so its files are never deleted,
/// and a stale record can never delete the files of a later launch that drew the same name. The claim goes last, so
/// the names stay reserved until the rest is gone, and stays if any other file could not be deleted.
///
/// Nothing is deleted when there is no claim, or when the claim file is held by another process, is another file than
/// the claim says, or is gone. With no claim nothing owns the files, so `warn` is told which are there. In the other
/// three someone else owns the cleanup (another deleter or an inheritor of the hold; the launch whose file it is; the
/// deleter of the claim, which goes after every other file, so what is there now is a later launch's) and nothing is
/// said. A file that is already gone is fine; a failure to look at or hold the claim, or to delete a file, goes to
/// `warn`.
pub(crate) fn remove_files(
    host: &dyn Host,
    paths: &[PathBuf],
    claim: Option<&FileClaim>,
    warn: &mut dyn FnMut(String),
) {
    let there = present(paths);
    if there.is_empty() {
        return;
    }
    let Some(claim) = claim else {
        warn(format!("not deleting {}: no launch claims them", names(&there)));
        return;
    };
    match host.hold_existing(&claim.path) {
        // The identity of the open handle: it cannot be absent, so the claim is the launch's or another's.
        Ok(HoldOutcome::Taken(hold)) => match hold.id() {
            Ok(id) if id == claim.id => remove_held(host, hold, &claim.path, paths, warn),
            Ok(_) => {}
            Err(e) => warn(format!("cannot look at {}: {e}", claim.path.display())),
        },
        Ok(HoldOutcome::Busy | HoldOutcome::Absent) => {}
        Err(e) => warn(format!("cannot hold {}: {e}", claim.path.display())),
    }
}

/// The files among `paths` that are there. A lookup that fails counts as there: the delete reports the system's error,
/// and the claim stays.
fn present(paths: &[PathBuf]) -> Vec<&PathBuf> {
    paths
        .iter()
        .filter(|path| !matches!(symlink_metadata_if_present(path), Ok(None)))
        .collect()
}

fn names(paths: &[&PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The deleting half of [`remove_files`]: `hold` holds `claim`, one of `paths`. Nothing is deleted or said when `claim`
/// no longer names the held file: whoever deleted it took the launch's other files first, and the name may be a later
/// launch's.
///
/// The socket and lock go before the claim, which is what leaves no socket without its claim. While the claim exists
/// only its holder deletes them; once it is gone, the sweep deletes a lock whose socket is gone too (`sweep_launch`).
pub(crate) fn remove_held(
    host: &dyn Host,
    hold: Box<dyn Hold>,
    claim: &Path,
    paths: &[PathBuf],
    warn: &mut dyn FnMut(String),
) {
    // The name is looked up on purpose: this asks whether the name still leads to the held file.
    match hold.id().and_then(|held| held.compare(host.file_id(claim))) {
        Ok(Comparison::Same) => {}
        Ok(Comparison::Different | Comparison::Absent) => return,
        Err(e) => {
            warn(format!("cannot look at {}: {e}", claim.display()));
            return;
        }
    }
    let mut all_gone = true;
    for path in paths.iter().filter(|path| path.as_path() != claim) {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if is_absent(&e) => {}
            Err(e) => {
                all_gone = false;
                warn(format!("cannot delete {}: {e}", path.display()));
            }
        }
    }
    if all_gone && let Err(e) = hold.delete() {
        warn(format!("cannot delete {}: {e}", claim.display()));
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MachineSpec {
    /// The instance this machine belongs to.
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
