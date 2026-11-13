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
use crate::{Arch, Error, ErrorKind, Network, Result};

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

    /// What `devvm doctor` asks about this backend: one line per piece the backend needs, in the order to show them. A
    /// line is a [`Check`] that passes or fails; none of them fails the call. A failing line is also what
    /// `availability` reports, so a backend states what is missing once. Nothing is downloaded or started. `detail`
    /// says what a passing line may spend to say what it found; whether it passes does not depend on it.
    fn checks(&self, detail: Detail) -> Vec<Check>;

    /// Whether this backend can save a running machine's memory.
    fn can_save_memory(&self) -> bool;
}

/// What a passing [`Check`] may spend to say what it found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Detail {
    /// Nothing that costs more than a look: what `availability` asks.
    Brief,
    /// Everything the line can say, a hash of a large file included: what `devvm doctor` prints.
    Full,
}

/// One line of `devvm doctor`: what was asked, and either what was found (`Ok`) or what is wrong (`Err`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub outcome: std::result::Result<String, String>,
}

/// The disk of a layer in its directory: every backend's layers hold it under this name, so a builder that fills a
/// layer's disk by running a machine on it can name the file.
pub(crate) const LAYER_DISK: &str = "disk.qcow2";

/// The instance disk size when the recipe sets none and the layer's disk is no bigger.
pub(crate) const DEFAULT_DISK: u64 = 40 << 30;

pub(crate) trait Storage: Send + Sync {
    /// Makes `into` a copy-on-write disk on top of `layer`. `Some(size)` smaller than the layer's disk (after the
    /// backend's size rounding), or one the backend cannot make, is `Recipe`, with nothing made. `None` is
    /// `DEFAULT_DISK` or the layer's disk, whichever is larger. A layer the backend cannot cover is `Store`; a size the
    /// backend cannot make is refused before the layer is looked at.
    fn create_instance_disk(&self, layer: &LayerDir, into: &Path, size: Option<u64>) -> Result<()>;

    /// Turns an instance's disk into a layer. Writes only into `staging` and never modifies a parent layer.
    fn commit_layer(&self, instance_disk: &Path, staging: &LayerDir) -> Result<()>;

    /// Makes the downloaded disk `image` (any format the backend reads) the root layer's disk in `staging`, with the
    /// firmware variables template for `arch`. Writes only into `staging`.
    fn import_base(&self, image: &Path, arch: Arch, staging: &LayerDir) -> Result<()>;

    /// Makes an empty disk of `size` bytes, the root layer's disk in `staging`, with the firmware variables template for
    /// `arch`. A `size` the backend cannot make is `Recipe`, with nothing made. Writes only into `staging`.
    fn create_blank_disk(&self, size: u64, arch: Arch, staging: &LayerDir) -> Result<()>;

    /// Makes `into` a copy-on-write disk over `backing`, a raw image that is only read, and writes each of `patches`
    /// into it: a read of the disk gives the patch's bytes where a patch lies and `backing`'s bytes elsewhere. Every
    /// request [`check_overlay`] refuses is refused with nothing made. A failure after that leaves no overlay of its
    /// own. Of several calls for one `into`, one makes it and the others fail with `AlreadyExists` ([`make_overlay`]); a
    /// call never deletes or changes an overlay another call made. The directory of `into` is a staging directory the
    /// caller owns: the call's temporaries (the overlay before it is published, the patch files) are made there, and a
    /// killed call's leftovers are the caller's to clean up.
    fn overlay_with_patches(&self, backing: &Path, patches: &[Patch], into: &Path) -> Result<()>;
}

/// The granularity of a patch: its offset and its length are multiples of it. QEMU's raw driver takes nothing finer.
pub(crate) const PATCH_ALIGNMENT: u64 = 512;

/// Bytes to write at `offset` into a disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Patch {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// The contract of [`Storage::overlay_with_patches`], for every backend to check before it makes anything:
/// - `backing` is an absolute path: a relative one would be resolved against the overlay's directory by one tool and
///   the working directory by another. `Internal`;
/// - `backing` is there: `NotFound`, naming it;
/// - every patch is non-empty, has an offset and a length that are multiples of [`PATCH_ALIGNMENT`], and lies within
///   `backing`'s size: `Internal`, naming the patch and the rule;
/// - `into` is not there: `AlreadyExists`.
pub(crate) fn check_overlay(backing: &Path, patches: &[Patch], into: &Path) -> Result<()> {
    if !backing.is_absolute() {
        return Err(Error::new(
            ErrorKind::Internal,
            format!("the backing file {} must be an absolute path", backing.display()),
        ));
    }
    let size = match std::fs::metadata(backing) {
        Ok(meta) => meta.len(),
        Err(e) if is_absent(&e) => {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("the backing file {} is not there", backing.display()),
            ));
        }
        Err(e) => {
            return Err(Error::io(
                ErrorKind::Io,
                format!("cannot look at the backing file {}", backing.display()),
                e,
            ));
        }
    };
    for (n, patch) in patches.iter().enumerate() {
        let len = patch.bytes.len() as u64;
        let wrong = if len == 0 {
            Some("it is empty".to_owned())
        } else if !patch.offset.is_multiple_of(PATCH_ALIGNMENT) || !len.is_multiple_of(PATCH_ALIGNMENT) {
            Some(format!(
                "its offset and its length must be multiples of {PATCH_ALIGNMENT}"
            ))
        } else if patch.offset.checked_add(len).is_none_or(|end| end > size) {
            Some(format!("it ends past the end of the backing file ({size} bytes)"))
        } else {
            None
        };
        if let Some(rule) = wrong {
            return Err(Error::new(
                ErrorKind::Internal,
                format!("patch {n} (offset {}, {len} bytes): {rule}", patch.offset),
            ));
        }
    }
    if symlink_metadata_if_present(into)
        .map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot look at the overlay {}", into.display()),
                e,
            )
        })?
        .is_some()
    {
        return Err(Error::new(
            ErrorKind::AlreadyExists,
            format!("the overlay {} is already there", into.display()),
        ));
    }
    Ok(())
}

/// The largest virtual size of a qcow2 disk: 2 PiB, the most `qemu-img create -f qcow2` makes with its default 64 KiB
/// clusters (an L2 table maps 512 MiB, and the L1 table holds at most 4 Mi tables). Past that, `qemu-img create` fails
/// with EFBIG. Every backend refuses a disk beyond it, as [`Storage::create_blank_disk`] says.
pub(crate) const QCOW2_MAX_SIZE: u64 = 1 << 51;

/// The size of the disk `qemu-img create` makes of a request for `size` bytes: rounded up to 512 bytes. `None` when
/// that is more than a qcow2 disk can have ([`QCOW2_MAX_SIZE`]).
pub(crate) fn made_size(size: u64) -> Option<u64> {
    size.checked_next_multiple_of(512)
        .filter(|made| *made <= QCOW2_MAX_SIZE)
}

/// The contract of [`Storage::create_blank_disk`], for every backend to check before it makes anything: `Recipe` when
/// no qcow2 disk is `size` bytes. The caller of a blank disk names its size, not `resources.disk`.
pub(crate) fn check_blank_disk_size(size: u64) -> Result<()> {
    made_size(size).map(drop).ok_or_else(|| {
        Error::new(
            ErrorKind::Recipe,
            format!("a blank disk of {size} bytes is more than the {QCOW2_MAX_SIZE} bytes a qcow2 disk can have"),
        )
    })
}

/// The contract of [`Backend::launch`] on the spec, for every backend to check before it starts anything: `NotSupported`
/// for an arm64 machine with a VGA adapter, whose firmware and guests have no driver for one.
pub(crate) fn check_machine_spec(spec: &MachineSpec) -> Result<()> {
    if spec.arch == Arch::Arm64 && spec.profile.video == Video::Vga {
        return Err(Error::new(
            ErrorKind::NotSupported,
            "an arm64 machine has no VGA adapter: its firmware and guests have no driver for one",
        ));
    }
    Ok(())
}

/// The directory that holds `path`, which is `what`. Every path devvm gives the backend is absolute and in a directory, so
/// a path without one is a bug in the caller, named here instead of resolved against the working directory.
pub(crate) fn directory_of<'a>(path: &'a Path, what: &str) -> Result<&'a Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Internal,
                format!("{what} {} has no directory to put its files in", path.display()),
            )
        })
}

/// Makes the overlay at `into`, for every backend, under the contract of [`Storage::overlay_with_patches`]. [`check_overlay`]
/// looks at `into` first, but another call can make it between that look and this one, so the look decides nothing:
/// `fill` is handed a new path in the directory of `into` and that directory, writes the overlay to the path (and any
/// temporary of its own into the directory), and the overlay goes to `into` in one step that fails with `AlreadyExists`
/// for anything there. Only the new path is ever deleted, on every failure of `fill` or of the publish.
///
/// The overlay is synced before it is published, and the directory after (Unix). The file is private to the account, as
/// a temporary file is (`0600` on Unix).
pub(crate) fn make_overlay(into: &Path, fill: impl FnOnce(&Path, &Path) -> Result<()>) -> Result<()> {
    let directory = directory_of(into, "the overlay")?;
    let staging = tempfile::Builder::new()
        .prefix("overlay-")
        .tempfile_in(directory)
        .map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot create a file in {} to make the overlay in", directory.display()),
                e,
            )
        })?
        // The handle is closed: `qemu-img` opens the file by name, which Windows lets a process do only once the other
        // handles are gone.
        .into_temp_path();
    let left = format!("the file {}", staging.display());
    let filled = fill(&staging, directory).and_then(|()| {
        // The handle `fill` used is closed, and the data must be on the disk before the name is: a crash must not leave
        // an overlay that is there and empty. Windows flushes only a handle opened for writing.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&staging)
            .and_then(|file| file.sync_all())
            .map_err(|e| {
                Error::io(
                    ErrorKind::Io,
                    format!("cannot flush the overlay {}", staging.display()),
                    e,
                )
            })
    });
    if let Err(e) = filled {
        return Err(e.after_cleanup(&left, staging.close()));
    }
    staging.persist_noclobber(into).map_err(|failed| {
        let tempfile::PathPersistError { error, path } = failed;
        let refused = if error.kind() == std::io::ErrorKind::AlreadyExists {
            Error::new(
                ErrorKind::AlreadyExists,
                format!("the overlay {} is already there", into.display()),
            )
        } else {
            Error::io(
                ErrorKind::Io,
                format!("cannot move the overlay into place at {}", into.display()),
                error,
            )
        };
        refused.after_cleanup(&left, path.close())
    })?;
    // The overlay is in place, so a directory that cannot be flushed takes it out again: the call made nothing.
    crate::host::current().sync_dir(directory).map_err(|e| {
        let error = Error::io(ErrorKind::Io, format!("cannot flush {}", directory.display()), e);
        error.after_cleanup(&format!("the overlay {}", into.display()), std::fs::remove_file(into))
    })
}

/// How a VM process ended, as the backend that started it tells it.
#[derive(Clone, Debug)]
pub(crate) struct MachineExit {
    pub status: std::process::ExitStatus,
    /// What the backend knows of this launch's end: for QEMU, what that launch printed. Empty when nothing is known.
    pub output: String,
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

    /// How the VM process ended if it has exited, `None` while it runs: std's `Child::try_wait`, with the backend's own
    /// account of the exit. `Internal` for a machine from `Backend::attach` or a released one, whose status this
    /// process cannot read, and for one that is not resumed.
    fn try_wait(&self) -> Result<Option<MachineExit>>;

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
    /// How QEMU reads `path`. Always named on the command line, never probed from the file's content.
    pub format: MediaFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaFormat {
    Raw,
    Qcow2,
}

impl MediaFormat {
    pub(crate) fn name(self) -> &'static str {
        match self {
            MediaFormat::Raw => "raw",
            MediaFormat::Qcow2 => "qcow2",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaKind {
    Cdrom,
    Disk,
    /// A removable USB mass-storage disk on the machine's one `qemu-xhci` controller.
    UsbDisk,
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
    pub video: Video,
    /// Do not reboot after a guest-initiated reset (installs).
    pub no_reboot: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiskBus {
    Virtio,
    Sata,
    Nvme,
}

/// The display adapter. Either way the machine has no display window (`-display none`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Video {
    None,
    Vga,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NetModel {
    Virtio,
    E1000,
}
