//! The QEMU backend against the contract every backend meets, and against QEMU's own behaviour.
//!
//! These machines run firmware only, on an empty qcow2: no guest OS boots, so they need the sidecar and WHPX and nothing
//! else. A firmware-only machine does not end on `system_powerdown` (the `POWERDOWN` event arrives, the process stays),
//! so the contract functions that wait for a graceful end are not run here.

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::os::{self, Watch};
use crate::backend::contract::{self, ContractTarget};
use crate::backend::qemu::backend::QemuBackend;
use crate::backend::qemu::qmp::{Qmp, QmpEndpoint};
use crate::backend::qemu::sidecar::QemuInstall;
use crate::backend::qemu::storage::{DISK, QemuStorage, VARS};
use crate::backend::{
    Backend, DeviceProfile, DiskBus, Endpoint, Firmware, Machine, MachineRecord, MachineSpec, NetModel, Storage,
};
use crate::contain::{Lifetime, VmProcess};
use crate::exit::StderrProgress;
use crate::host::{self, Host, ProcessWatch};
use crate::instance::record::InstanceId;
use crate::store::layers::layers_tests::{key, request};
use crate::store::layers::{Built, ChainLease, LayerStore};
use crate::test_support::fixtures::{MemoryProgress, UNICODE_DIR, unicode_home};
use crate::test_support::host::{NO_GUEST_RESET, WINDOWS_11};
use crate::test_support::qemu::{lazy_at, qemu_img, qemu_sidecar, socket_dir};
use crate::{Arch, ErrorKind, Network, Result};

// The target ==========================================================================================================

/// What a test needs to know of each machine it launched through a [`QemuTarget`], by instance: a process handle (`.0`)
/// and where its control socket is (`.1`; every launch has its own, so it is known only from the machine's record).
#[derive(Default)]
struct Watches(
    Mutex<HashMap<InstanceId, Watch>>,
    Mutex<HashMap<InstanceId, QmpEndpoint>>,
);

impl Watches {
    fn launched(&self, id: InstanceId, record: &MachineRecord) {
        if let Some(watch) = Watch::new(record.process.pid) {
            self.0.lock().unwrap().insert(id, watch);
        }
        self.1.lock().unwrap().insert(id, QmpEndpoint::from_record(&record.qmp));
    }
}

/// A [`QemuBackend`] that keeps a process handle of every machine launched through it, so that the exit code is known
/// after the process is gone (a graceful end exits 0, a kill exits 1).
struct Observing {
    inner: QemuBackend,
    watches: Arc<Watches>,
}

impl Backend for Observing {
    fn launch(&self, spec: &MachineSpec) -> Result<Box<dyn Machine>> {
        let machine = self.inner.launch(spec)?;
        self.watches.launched(spec.id, &machine.record());
        Ok(machine)
    }

    fn attach(&self, rec: &MachineRecord) -> Result<Box<dyn Machine>> {
        self.inner.attach(rec)
    }

    fn storage(&self) -> &dyn Storage {
        self.inner.storage()
    }

    fn can_save_memory(&self) -> bool {
        self.inner.can_save_memory()
    }
}

fn backend_over(bin: &Path, host: &'static dyn Host) -> QemuBackend {
    let install = lazy_at(bin);
    let storage = Arc::new(QemuStorage::new(install.clone(), host));
    QemuBackend::new(install, storage, host, socket_dir(), Arc::new(StderrProgress))
}

/// A spec for a firmware-only machine in `dir`, on the host's accelerator.
fn spec_in(dir: &Path, id: InstanceId, lifetime: Lifetime) -> MachineSpec {
    let accel = host::current()
        .accelerator(Arch::Amd64)
        .unwrap_or_else(|e| panic!("the VM tests need WHPX: {e}"));
    MachineSpec {
        id,
        dir: dir.to_path_buf(),
        arch: Arch::Amd64,
        accel,
        cpus: 1,
        memory: 512 << 20,
        network: Network::Nat,
        disk: dir.join("disk.img"),
        media: Vec::new(),
        profile: DeviceProfile {
            disk: DiskBus::Virtio,
            net: NetModel::Virtio,
            no_reboot: false,
        },
        firmware: None,
        forwards: vec![Endpoint::Normal],
        serial_log: dir.join("serial.log"),
        lifetime,
    }
}

struct QemuTarget {
    backend: Arc<Observing>,
    install: Arc<QemuInstall>,
    watches: Arc<Watches>,
}

impl QemuTarget {
    fn new(install: &Arc<QemuInstall>) -> QemuTarget {
        QemuTarget::on(install, host::current())
    }

    fn on(install: &Arc<QemuInstall>, host: &'static dyn Host) -> QemuTarget {
        let watches = Arc::new(Watches::default());
        QemuTarget {
            backend: Arc::new(Observing {
                inner: backend_over(&install.bin, host),
                watches: watches.clone(),
            }),
            install: install.clone(),
            watches,
        }
    }

    /// The control socket of the machine last launched from `spec`.
    fn endpoint(&self, spec: &MachineSpec) -> QmpEndpoint {
        self.watches.1.lock().unwrap()[&spec.id].clone()
    }

    /// The machine `n` under `root`, its disk made by the backend's own storage over a layer.
    fn prepared(&self, root: &Path, n: u32, edit: impl FnOnce(&mut MachineSpec)) -> (MachineSpec, ChainLease) {
        let world = crate::store::layers::layers_tests::env(root);
        let parent = self.parent_layer(&world.store);
        let mut spec = self.spec(root, n);
        edit(&mut spec);
        std::fs::create_dir_all(&spec.dir).unwrap();
        self.backend
            .storage()
            .create_instance_disk(parent.tip().dir(), &spec.disk, 1 << 20)
            .unwrap();
        (spec, parent)
    }
}

impl ContractTarget for QemuTarget {
    fn backend(&self) -> Arc<dyn Backend> {
        self.backend.clone()
    }

    /// A layer holding an empty qcow2 and the UEFI variables template: firmware boots, finds nothing to boot, and waits.
    fn parent_layer(&self, store: &LayerStore) -> ChainLease {
        store
            .build(&request(key(1)), None, |staging| {
                let disk = staging.dir.path().join(DISK);
                qemu_img(
                    &self.install,
                    &[
                        "create".as_ref(),
                        "-f".as_ref(),
                        "qcow2".as_ref(),
                        disk.as_os_str(),
                        "1M".as_ref(),
                    ],
                );
                std::fs::copy(
                    self.install.share.join("edk2-i386-vars.fd"),
                    staging.dir.path().join(VARS),
                )
                .unwrap();
                Ok(Built::default())
            })
            .unwrap()
    }

    fn spec(&self, root: &Path, n: u32) -> MachineSpec {
        spec_in(
            &root.join(format!("machine-{n}")),
            InstanceId::random(),
            Lifetime::OutlivesOwner,
        )
    }

    fn launch_elsewhere(&self, spec: &MachineSpec) -> Result<MachineRecord> {
        let output = crate::test_support::helper::command(
            "qemu-launcher",
            &[
                &self.install.bin.display().to_string(),
                &spec.dir.display().to_string(),
                &spec.id.to_string(),
            ],
        )
        .output()
        .expect("the launcher should run");
        assert!(
            output.status.success(),
            "the launcher failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let record: MachineRecord = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
            panic!(
                "the launcher printed no machine record ({e}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        self.watches.launched(spec.id, &record);
        Ok(record)
    }

    fn shutdown_elsewhere(&self, record: &MachineRecord) -> Result<()> {
        let output = crate::test_support::helper::command(
            "qemu-shutdown",
            &[
                &self.install.bin.display().to_string(),
                &serde_json::to_string(record).unwrap(),
            ],
        )
        .output()
        .expect("the shutdown helper should run");
        if output.status.success() {
            Ok(())
        } else {
            Err(crate::Error::new(
                ErrorKind::Internal,
                format!(
                    "the shutdown helper failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ),
            ))
        }
    }

    fn ended_gracefully(&self, spec: &MachineSpec) -> bool {
        self.watches
            .0
            .lock()
            .unwrap()
            .get(&spec.id)
            .and_then(Watch::exit_code)
            .is_some_and(|code| code == 0)
    }

    fn has_run(&self, spec: &MachineSpec) -> bool {
        self.endpoint(spec).path().exists() || std::fs::metadata(&spec.serial_log).is_ok_and(|log| log.len() > 0)
    }

    fn leftovers(&self, spec: &MachineSpec) -> Vec<PathBuf> {
        let endpoint = self.endpoint(spec);
        vec![endpoint.path().to_path_buf(), endpoint.lock_path()]
    }
}

/// Helper `qemu-launcher <qemu bin> <instance dir> <instance id>`: launches an `OutlivesOwner` machine, resumes and
/// releases it, prints its record and exits, leaving the machine running. The disk was made by the test.
pub(crate) fn qemu_launcher(args: &[String]) {
    let backend = backend_over(Path::new(&args[0]), host::current());
    let spec = spec_in(
        Path::new(&args[1]),
        args[2].parse().expect("the instance id"),
        Lifetime::OutlivesOwner,
    );
    let mut machine = backend.launch(&spec).expect("the machine should launch");
    machine.resume().expect("the machine should resume");
    machine.release().expect("the machine should release");
    println!("{}", serde_json::to_string(&machine.record()).unwrap());
}

/// Helper `qemu-shutdown <qemu bin> <machine record json>`: attaches to the machine and shuts it down.
pub(crate) fn qemu_shutdown(args: &[String]) {
    let backend = backend_over(Path::new(&args[0]), host::current());
    let record: MachineRecord = serde_json::from_str(&args[1]).expect("the argument is a machine record");
    backend
        .attach(&record)
        .expect("the machine should be attachable")
        .shutdown()
        .expect("the machine should shut down");
}

// Helpers =============================================================================================================

fn is_running(record: &MachineRecord) -> bool {
    VmProcess::attach(&record.process).unwrap().is_some()
}

/// The watch of the machine's process, as a session needs one.
fn watch_of(record: &MachineRecord) -> Arc<dyn ProcessWatch> {
    let vm = VmProcess::attach(&record.process)
        .unwrap()
        .expect("the machine's process runs");
    host::current().watch_process(vm.id()).unwrap()
}

/// The host port QEMU has bound for the machine's first forward, read over QMP.
fn forwarded_host_port(record: &MachineRecord) -> u16 {
    let endpoint = QmpEndpoint::from_record(&record.qmp);
    let mut qmp = Qmp::connect(&endpoint, &watch_of(record), &MemoryProgress::default()).unwrap();
    qmp.forwarded_ports().unwrap()[0].host
}

fn run_state(record: &MachineRecord) -> String {
    let endpoint = QmpEndpoint::from_record(&record.qmp);
    let mut qmp = Qmp::connect(&endpoint, &watch_of(record), &MemoryProgress::default()).unwrap();
    qmp.query_status().unwrap().state
}

// The contract ========================================================================================================

#[skuld::test]
fn vm_qemu_launch_reports_a_live_process_and_loopback_endpoints(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    contract::launch_reports_a_live_process_and_loopback_endpoints(&QemuTarget::new(install), home);
}

#[skuld::test]
fn vm_qemu_a_frozen_machine_has_not_run_and_kill_leaves_nothing(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    contract::a_frozen_machine_has_not_run_and_kill_leaves_nothing(&QemuTarget::new(install), home);
}

#[skuld::test]
fn vm_qemu_a_released_machine_outlives_its_launcher_and_attach_finds_it(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    contract::a_released_machine_outlives_its_launcher_and_attach_finds_it(&QemuTarget::new(install), home);
}

#[skuld::test]
fn vm_qemu_kill_returns_only_after_the_process_has_exited(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    contract::kill_returns_only_after_the_process_has_exited(&QemuTarget::new(install), home);
}

#[skuld::test]
fn vm_qemu_attach_to_a_dead_machine_fails(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    contract::attach_to_a_dead_machine_fails(&QemuTarget::new(install), home);
}

#[skuld::test]
fn vm_qemu_storage_writes_only_into_staging(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    contract::storage_writes_only_into_staging(&QemuTarget::new(install), home);
}

#[skuld::test]
fn vm_qemu_concurrent_launches_get_distinct_processes_and_endpoints(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    contract::concurrent_launches_get_distinct_processes_and_endpoints(&QemuTarget::new(install), home);
}

// QEMU only ===========================================================================================================

#[skuld::test]
fn vm_qemu_spawned_through_the_backend_has_no_console_and_no_window(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let (spec, _parent) = target.prepared(home, 0, |_| {});
    let mut machine = target.backend().launch(&spec).unwrap();
    machine.resume().unwrap();
    let pid = machine.record().process.pid;

    // A process with a console has a `conhost.exe` child; a detached one has none. QEMU has no children of its own.
    assert_eq!(os::children_of(pid), 0, "QEMU has a console (a child process)");
    assert_eq!(os::windows_of(pid), 0, "QEMU has a window");
    machine.kill().unwrap();
}

#[skuld::test]
fn vm_qemu_a_killed_frozen_machine_leaves_no_socket_file_no_lock_file_and_no_serial_content(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let (spec, _parent) = target.prepared(home, 0, |_| {});
    let mut machine = target.backend().launch(&spec).unwrap();

    machine.kill().unwrap();

    for leftover in target.leftovers(&spec) {
        assert!(!leftover.exists(), "{} was left", leftover.display());
    }
    assert!(!spec.serial_log.exists() || std::fs::metadata(&spec.serial_log).unwrap().len() == 0);
}

#[skuld::test]
fn vm_qemu_deleting_an_earlier_machines_leftovers_leaves_the_next_launches_files(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let (spec, _parent) = target.prepared(home, 0, |_| {});
    let mut first = target.backend().launch(&spec).unwrap();
    let first_record = first.record();
    first.kill().unwrap();
    let mut second = target.backend().launch(&spec).unwrap();
    let second_record = second.record();
    assert_ne!(
        first_record.qmp, second_record.qmp,
        "a launch has its own control socket"
    );
    for path in &second_record.leftovers {
        std::fs::write(path, b"second").unwrap();
    }

    // What a late `Vm::drop` of the first machine does.
    first_record.remove_leftovers(&mut |message| panic!("{message}"));

    for path in &second_record.leftovers {
        assert!(path.exists(), "{} was deleted", path.display());
    }
    second.kill().unwrap();
}

#[skuld::test]
fn vm_qemu_start_failure_reports_qemu_log(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let missing = home.join("no-such-firmware.fd");
    let (spec, _parent) = target.prepared(home, 0, |spec| {
        spec.firmware = Some(Firmware {
            code: missing.clone(),
            vars: None,
        });
    });
    let mut machine = target.backend().launch(&spec).unwrap();

    let error = machine.resume().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(
        error.message().contains("no-such-firmware.fd"),
        "QEMU's own message: {error}"
    );
    assert!(error.message().contains("qemu.log"), "{error}");
    assert!(!is_running(&machine.record()), "QEMU exited");
}

#[skuld::test]
fn vm_qemu_launch_with_paths_under_space_and_non_ascii(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let (spec, _parent) = target.prepared(home, 0, |_| {});
    assert!(spec.disk.to_string_lossy().contains(UNICODE_DIR));
    let mut machine = target.backend().launch(&spec).unwrap();
    assert!(target.endpoint(&spec).path().to_string_lossy().contains(UNICODE_DIR));

    machine.resume().unwrap();

    assert!(is_running(&machine.record()));
    assert!(machine.ssh_endpoints()[0].1.ip().is_loopback());
    machine.kill().unwrap();
}

#[skuld::test]
fn vm_qemu_a_running_machine_leaves_no_leftovers_after_kill(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let (spec, _parent) = target.prepared(home, 0, |_| {});
    let mut machine = target.backend().launch(&spec).unwrap();
    machine.resume().unwrap();
    for leftover in target.leftovers(&spec) {
        assert!(leftover.exists(), "a running machine has {}", leftover.display());
    }

    machine.kill().unwrap();

    for leftover in target.leftovers(&spec) {
        assert!(!leftover.exists(), "kill left {}", leftover.display());
    }
    // `ended_gracefully` is false for a process nobody watched, so the exit code must have been seen.
    let code = target.watches.0.lock().unwrap()[&spec.id].exit_code();
    assert!(code.is_some(), "no exit code was observed after the kill");
    assert!(
        !target.ended_gracefully(&spec),
        "a kill is not a graceful end: {code:?}"
    );
}

#[skuld::test]
fn vm_qemu_release_before_resume_is_internal_and_on_an_attached_machine_is_ok(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let (spec, _parent) = target.prepared(home, 0, |_| {});
    let mut machine = target.backend().launch(&spec).unwrap();
    assert_eq!(machine.release().unwrap_err().kind(), ErrorKind::Internal);

    machine.resume().unwrap();
    let mut attached = target.backend().attach(&machine.record()).unwrap();
    attached.release().expect("releasing an attached machine is Ok");
    assert_eq!(
        attached.resume().unwrap_err().kind(),
        ErrorKind::Internal,
        "it already runs"
    );
    attached.kill().unwrap();
}

#[skuld::test]
fn vm_qemu_a_frozen_machine_cannot_be_reset_or_shut_down_and_does_not_hang(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let (spec, _parent) = target.prepared(home, 0, |_| {});
    let mut machine = target.backend().launch(&spec).unwrap();

    assert_eq!(machine.reset().unwrap_err().kind(), ErrorKind::Internal);
    assert_eq!(machine.shutdown().unwrap_err().kind(), ErrorKind::Internal);
    machine.kill().unwrap();
}

// Reset ---------------------------------------------------------------------------------------------------------------

#[skuld::test]
fn vm_qemu_a_reset_of_a_no_reboot_machine_is_refused_and_leaves_it_running(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let (spec, _parent) = target.prepared(home, 0, |spec| spec.profile.no_reboot = true);
    let mut machine = target.backend().launch(&spec).unwrap();
    machine.resume().unwrap();
    let record = machine.record();
    assert!(record.no_reboot);
    let mut attached = target.backend().attach(&record).unwrap();

    for (who, error) in [
        ("launched", machine.reset().unwrap_err()),
        ("attached", attached.reset().unwrap_err()),
    ] {
        assert_eq!(error.kind(), ErrorKind::Unavailable, "{who}: {error}");
        assert!(error.message().contains("no-reboot"), "{who}: {error}");
    }

    assert!(is_running(&record), "nothing was sent that could end QEMU");
    assert_eq!(run_state(&record), "running");
    machine.kill().unwrap();
}

#[skuld::test]
fn vm_qemu_a_system_reset_that_reaches_a_no_reboot_qemu_reports_no_reboot_and_the_process_ends(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let (spec, _parent) = target.prepared(home, 0, |spec| spec.profile.no_reboot = true);
    let mut machine = target.backend().launch(&spec).unwrap();
    machine.resume().unwrap();
    // A record from before the flag existed does not say the machine cannot reset, so the request reaches QEMU.
    let record = MachineRecord {
        no_reboot: false,
        ..machine.record()
    };
    let mut attached = target.backend().attach(&record).unwrap();

    let error = attached.reset().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("no-reboot"), "{error}");
    assert!(!is_running(&record), "QEMU exits at its reset under -no-reboot");
    assert_eq!(
        target.watches.0.lock().unwrap()[&spec.id].exit_code(),
        Some(0),
        "and exits normally"
    );
}

#[skuld::test]
fn vm_qemu_a_system_reset_without_no_reboot_keeps_the_process_and_its_forwards(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let (spec, _parent) = target.prepared(home, 0, |_| {});
    let mut machine = target.backend().launch(&spec).unwrap();
    machine.resume().unwrap();
    let before = machine.record();
    let port = machine.ssh_endpoints()[0].1;

    machine.reset().unwrap();

    let after = machine.record();
    assert_eq!(after.process, before.process, "the same process");
    assert!(is_running(&after));
    assert_eq!(machine.ssh_endpoints()[0].1, port);
    assert_eq!(
        SocketAddr::from(([127, 0, 0, 1], forwarded_host_port(&after))),
        port,
        "QEMU's forward is unchanged"
    );
    assert_eq!(run_state(&after), "running");
    machine.kill().unwrap();
}

#[skuld::test]
fn vm_qemu_a_machine_found_with_attach_can_be_reset(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let (spec, _parent) = target.prepared(home, 0, |_| {});
    let mut machine = target.backend().launch(&spec).unwrap();
    machine.resume().unwrap();
    let mut attached = target.backend().attach(&machine.record()).unwrap();

    attached.reset().unwrap();

    assert_eq!(run_state(&machine.record()), "running");
    machine.kill().unwrap();
}

#[skuld::test]
fn vm_qemu_reset_on_a_host_without_guest_reset_is_unavailable_naming_windows_11_and_leaves_the_machine_running(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::on(install, &NO_GUEST_RESET);
    // Under -no-reboot a reset that reached QEMU would end the process, so a running machine proves none was sent.
    let (spec, _parent) = target.prepared(home, 0, |spec| spec.profile.no_reboot = true);
    let mut machine = target.backend().launch(&spec).unwrap();
    machine.resume().unwrap();

    let error = machine.reset().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert_eq!(error.message(), WINDOWS_11);
    assert!(is_running(&machine.record()), "no system_reset reached QEMU");
    assert_eq!(run_state(&machine.record()), "running");
    machine.kill().unwrap();
}

#[skuld::test]
fn vm_qemu_whpx_error_lines_in_qemu_log_are_not_a_failure(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let (spec, _parent) = target.prepared(home, 0, |_| {});
    let mut machine = target.backend().launch(&spec).unwrap();
    // What healthy resets write: QEMU keeps running.
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(spec.dir.join("qemu.log"))
        .unwrap();
    writeln!(log, "WHPX: Failed to set register 00002095, hr=c0350005").unwrap();
    writeln!(log, "failed to get xsave state: Input/output error").unwrap();
    log.flush().unwrap();

    machine.resume().unwrap();
    machine.reset().unwrap();

    assert!(is_running(&machine.record()));
    assert_eq!(run_state(&machine.record()), "running");
    machine.kill().unwrap();
}

#[skuld::test]
fn vm_qemu_shutdown_of_a_paused_machine_fails_naming_the_state_and_returns(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let (spec, _parent) = target.prepared(home, 0, |_| {});
    let mut machine = target.backend().launch(&spec).unwrap();
    machine.resume().unwrap();
    let record = machine.record();
    {
        let endpoint = QmpEndpoint::from_record(&record.qmp);
        Qmp::connect(&endpoint, &watch_of(&record), &MemoryProgress::default())
            .unwrap()
            .stop()
            .unwrap();
    }

    let error = machine.shutdown().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().contains("paused"), "{error}");
    assert!(
        error.message().contains(&spec.serial_log.display().to_string()),
        "{error}"
    );
    assert!(is_running(&record), "the process is alive until the test kills it");
    machine.kill().unwrap();
}

#[skuld::test]
fn vm_qemu_reset_of_a_paused_machine_fails_naming_the_state_and_returns(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let target = QemuTarget::new(install);
    let (spec, _parent) = target.prepared(home, 0, |_| {});
    let mut machine = target.backend().launch(&spec).unwrap();
    machine.resume().unwrap();
    {
        let record = machine.record();
        let endpoint = QmpEndpoint::from_record(&record.qmp);
        Qmp::connect(&endpoint, &watch_of(&record), &MemoryProgress::default())
            .unwrap()
            .stop()
            .unwrap();
    }

    let error = machine.reset().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().contains("paused"), "{error}");
    machine.kill().unwrap();
}
