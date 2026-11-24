use std::path::Path;
use std::sync::Arc;

use super::QemuBackend;
use crate::backend::fake::fake_spec;
use crate::backend::qemu::args::ArchFiles;
use crate::backend::qemu::qmp::QmpEndpoint;
use crate::backend::qemu::sidecar::QemuInstall;
use crate::backend::qemu::storage::QemuStorage;
use crate::contain::Lifetime;
use crate::host::{HeldKind, Host};
use crate::progress::NullProgress;
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::helper;
use crate::test_support::host::RecordingHost;
use crate::test_support::qemu::{lazy_at, scratch_run};

/// What a launch started, as the host saw the files given to it.
struct Launched {
    /// The PID of the frozen process the launch started.
    pid: u32,
    /// The identity of the open file that is the launch's claim.
    claim: crate::host::FileId,
    host: &'static RecordingHost,
}

/// Runs the launch step of a backend on `install`, whose tool is an executable that is started frozen and never
/// resumed. The process is spawned, so this runs in CI only.
fn launch_on(home: &Path, install: &QemuInstall) -> Launched {
    let host: &'static RecordingHost = Box::leak(Box::new(RecordingHost::new()));
    let lazy = lazy_at(&install.bin);
    let storage = Arc::new(QemuStorage::new(lazy.clone(), host));
    let run = scratch_run();
    let backend = QemuBackend::new(
        lazy,
        storage,
        host,
        Ok(run.path().to_path_buf()),
        Arc::new(MemoryProgress::default()),
    );
    let reservation = QmpEndpoint::reserve(host, run.path(), &NullProgress).unwrap();
    let claim = reservation.claim.id;
    let spec = fake_spec(
        home.join("inst").join("disk.qcow2"),
        home.join("serial.log"),
        Lifetime::DiesWithOwner,
    );
    std::fs::create_dir_all(&spec.dir).unwrap();
    let mut started = None;

    backend
        .start(&spec, install, &reservation.endpoint, reservation.hold, &mut started)
        .unwrap();

    let pid = started.as_ref().expect("the process was started").id().pid();
    Launched { pid, claim, host }
}

/// An install whose `bin` has a `qemu-system` that is a copy of the test executable.
fn install_in(home: &Path, host: &dyn Host, held: bool) -> QemuInstall {
    let bin = home.join("install").join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let install = QemuInstall {
        bin: bin.clone(),
        share: home.join("install").join("share"),
        version: "configured".to_owned(),
        zip_sha256: None,
        held: None,
    };
    std::fs::copy(helper::exe(), install.tool(&ArchFiles::of(crate::Arch::Amd64).system)).unwrap();
    QemuInstall {
        held: held.then(|| Arc::new(host.open_held_read(&bin, HeldKind::Directory).unwrap())),
        ..install
    }
}

#[skuld::test]
fn a_launch_gives_the_started_process_the_installs_hold_and_its_own(#[fixture(unicode_home)] home: &Path) {
    let install = install_in(home, crate::host::current(), true);
    let held = crate::host::current()
        .file_id_of_open(install.held.as_ref().unwrap())
        .unwrap();

    let launched = launch_on(home, &install);

    let shared = launched.host.shared_files();
    assert!(shared.contains(&(held, launched.pid)), "the install's hold: {shared:?}");
    assert!(
        shared.contains(&(launched.claim, launched.pid)),
        "the launch's own hold: {shared:?}"
    );
}

#[skuld::test]
fn a_launch_of_a_configured_qemu_gives_the_started_process_no_install_hold(#[fixture(unicode_home)] home: &Path) {
    let install = install_in(home, crate::host::current(), false);

    let launched = launch_on(home, &install);

    let shared = launched.host.shared_files();
    assert_eq!(shared, [(launched.claim, launched.pid)], "only the launch's own hold");
}
