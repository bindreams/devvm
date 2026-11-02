use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::Dirs;
use crate::backend::Backend;
use crate::backend::qemu::backend::QemuBackend;
use crate::backend::qemu::qmp::QmpEndpoint;
use crate::backend::qemu::storage::QemuStorage;
use crate::host::{self, Hold, HoldOutcome};
use crate::instance::registry::Registry;
use crate::store::lock::{FileLock, Mode};
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::qemu::{lazy_at, scratch_run};

fn backend_in(run: &Path, progress: Arc<MemoryProgress>) -> Arc<QemuBackend> {
    let install = lazy_at(Path::new("no-qemu-here"));
    let storage = Arc::new(QemuStorage::new(install.clone(), host::current()));
    Arc::new(QemuBackend::new(
        install,
        storage,
        host::current(),
        run.to_path_buf(),
        progress,
    ))
}

fn sweep(run: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    backend_in(run, Arc::default()).sweep(&mut |text| warnings.push(text));
    warnings
}

/// A run directory with its `runtime.lock`, as a first launch leaves it.
fn run_with_a_lock() -> crate::test_support::qemu::ScratchRun {
    let run = scratch_run();
    drop(FileLock::open(&run.path().join("runtime.lock")).unwrap());
    run
}

/// The files of a machine named `stem` in `run`: its socket, the lock of its sessions and its hold. A machine is alive
/// while the hold that comes back is; a dropped one is the hold of a machine that has died.
struct Machine {
    sock: PathBuf,
    lock: PathBuf,
    hold_file: PathBuf,
    alive: Option<Box<dyn Hold>>,
}

impl Machine {
    fn new(run: &Path, stem: &str) -> Machine {
        let endpoint = QmpEndpoint::from_record(&run.join(format!("{stem}.sock")).display().to_string());
        std::fs::write(endpoint.path(), b"socket").unwrap();
        std::fs::write(endpoint.lock_path(), b"").unwrap();
        let alive = host::current().hold_new(&endpoint.hold_path()).unwrap();
        Machine {
            sock: endpoint.path().to_path_buf(),
            lock: endpoint.lock_path(),
            hold_file: endpoint.hold_path(),
            alive: Some(alive),
        }
    }

    fn dies(&mut self) {
        self.alive = None;
    }

    fn files(&self) -> [&PathBuf; 3] {
        [&self.sock, &self.lock, &self.hold_file]
    }

    fn all_exist(&self) -> bool {
        self.files().iter().all(|file| file.exists())
    }

    fn none_exist(&self) -> bool {
        self.files().iter().all(|file| !file.exists())
    }
}

const STEM_A: &str = "00000000000000aa";
const STEM_B: &str = "00000000000000bb";

#[skuld::test]
fn sweep_deletes_the_files_of_a_machine_that_is_dead() {
    let run = run_with_a_lock();
    let mut machine = Machine::new(run.path(), STEM_A);
    machine.dies();

    assert_eq!(sweep(run.path()), Vec::<String>::new());

    assert!(machine.none_exist());
}

#[skuld::test]
fn sweep_keeps_the_files_of_a_machine_that_is_alive_and_takes_them_once_it_is_dead() {
    let run = run_with_a_lock();
    let mut machine = Machine::new(run.path(), STEM_A);

    assert_eq!(sweep(run.path()), Vec::<String>::new(), "a live machine is no problem");
    assert!(machine.all_exist());

    machine.dies();
    sweep(run.path());
    assert!(machine.none_exist());
}

#[skuld::test]
fn sweep_takes_the_dead_and_leaves_the_alive_among_several() {
    let run = run_with_a_lock();
    let mut dead = Machine::new(run.path(), STEM_A);
    let alive = Machine::new(run.path(), STEM_B);
    dead.dies();

    sweep(run.path());

    assert!(dead.none_exist());
    assert!(alive.all_exist());
}

/// The lock a late `connect` creates after a shutdown has deleted the machine's files.
#[skuld::test]
fn a_session_lock_recreated_after_cleanup_is_swept_by_the_next_reap() {
    let run = run_with_a_lock();
    let lock = run.path().join(format!("{STEM_A}.sock.lock"));
    std::fs::write(&lock, b"").unwrap();

    assert_eq!(sweep(run.path()), Vec::<String>::new());

    assert!(!lock.exists());
}

/// A socket with no claim file beside it is nobody's that this sweep can tell is dead, so it stays.
#[skuld::test]
fn sweep_leaves_a_socket_that_has_no_claim_file() {
    let run = run_with_a_lock();
    let sock = run.path().join(format!("{STEM_A}.sock"));
    let lock = run.path().join(format!("{STEM_A}.sock.lock"));
    std::fs::write(&sock, b"socket").unwrap();
    std::fs::write(&lock, b"").unwrap();

    sweep(run.path());

    assert!(sock.exists() && lock.exists());
}

/// A launch whose reservation has returned holds its claim, which QEMU will inherit: a sweep finds it `Busy` and
/// leaves it, and takes the dead machine beside it.
#[skuld::test]
fn sweep_leaves_a_reserved_launch_that_has_not_connected_and_takes_a_dead_machine() {
    let run = run_with_a_lock();
    let mut dead = Machine::new(run.path(), STEM_A);
    dead.dies();
    let launching = QmpEndpoint::reserve(host::current(), run.path()).unwrap();

    assert_eq!(sweep(run.path()), Vec::<String>::new());

    assert!(dead.none_exist());
    assert!(launching.claim.path.exists());
}

/// A launch killed between making its claim file under the private name and linking it leaves that name.
#[skuld::test]
fn sweep_deletes_the_private_name_a_launch_left_when_it_died() {
    let run = run_with_a_lock();
    let leftover = run.path().join(format!("{STEM_A}.sock.hold.new"));
    std::fs::write(&leftover, b"").unwrap();

    assert_eq!(sweep(run.path()), Vec::<String>::new());

    assert!(!leftover.exists());
}

#[skuld::test]
fn sweep_skips_everything_while_another_sweep_runs() {
    let run = run_with_a_lock();
    let mut dead = Machine::new(run.path(), STEM_A);
    dead.dies();
    let mut other = FileLock::open(&run.path().join("runtime.lock")).unwrap();
    assert!(other.try_lock(Mode::Exclusive).unwrap());

    assert_eq!(sweep(run.path()), Vec::<String>::new());

    assert!(dead.all_exist());
}

#[skuld::test]
fn sweep_leaves_names_of_another_shape_alone() {
    let run = run_with_a_lock();
    let odd = [
        "dvm-0123456789abcdef0123456789abcdef-1.sock",
        "dvm-0123456789abcdef0123456789abcdef-1.sock.lock",
        "00000000000000a.sock",
        "00000000000000aaa.sock",
        "00000000000000AA.sock",
        "00000000000000AA.sock.lock",
        "00000000000000aa.sock.old",
        "00000000000000aa.hold",
        "00000000000000a.sock.hold.new",
        "00000000000000aa.sock.new",
        "notes.txt",
    ];
    for name in odd {
        std::fs::write(run.path().join(name), b"mine").unwrap();
    }
    // A directory with a name of the shape is not a file of ours either.
    let directory = run.path().join(format!("{STEM_B}.sock"));
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("inside"), b"mine").unwrap();

    assert_eq!(sweep(run.path()), Vec::<String>::new());

    for name in odd {
        assert_eq!(std::fs::read(run.path().join(name)).unwrap(), b"mine", "{name}");
    }
    assert!(directory.join("inside").exists());
    assert!(run.path().join("runtime.lock").exists());
}

#[skuld::test]
fn sweep_creates_nothing_when_the_run_dir_or_the_runtime_lock_is_absent(#[fixture(unicode_home)] home: &Path) {
    let missing = home.join("run");
    assert_eq!(sweep(&missing), Vec::<String>::new());
    assert!(!missing.exists(), "the run directory is not made");

    let run = scratch_run();
    assert_eq!(sweep(run.path()), Vec::<String>::new());
    assert!(
        !run.path().join("runtime.lock").exists(),
        "the lock of the directory is not made"
    );
    assert_eq!(std::fs::read_dir(run.path()).unwrap().count(), 0);
}

/// Two devvm setups with their own state directories share the user's runtime directory. Neither's `reap` may delete a
/// machine the other started.
#[skuld::test]
fn two_registries_over_one_run_dir_never_delete_each_others_live_socket(#[fixture(unicode_home)] home: &Path) {
    let run = run_with_a_lock();
    let (a, b) = (Dirs::under(&home.join("a")), Dirs::under(&home.join("b")));
    let progress = Arc::new(MemoryProgress::default());
    let registry = |dirs: &Dirs| {
        let backend: Arc<dyn Backend> = backend_in(run.path(), progress.clone());
        Registry::new(dirs, progress.clone(), Some(backend), host::current())
    };
    let (registry_a, registry_b) = (registry(&a), registry(&b));
    let mut machine = Machine::new(run.path(), STEM_A);

    for registry in [&registry_a, &registry_b] {
        let report = registry.reap();
        assert_eq!(report.warnings, Vec::<String>::new());
        assert!(machine.all_exist(), "a live machine's files are never deleted");
    }
    assert!(matches!(
        host::current().hold_existing(&machine.hold_file).unwrap(),
        HoldOutcome::Busy
    ));

    machine.dies();
    registry_a.reap();
    assert!(
        machine.none_exist(),
        "once the machine is dead, either reap removes its files"
    );
    registry_b.reap();
}
