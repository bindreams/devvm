use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::backend::fake::FakeBackend;
use crate::backend::{Backend, Endpoint, Machine, MachineRecord, MachineSpec, Storage};
use crate::contain::VmProcess;
use crate::contain::contain_tests::Sleeper;
use crate::engine::engine_tests::{
    EngineFixture, engine_fixture, engine_fixture_from, engine_fixture_with, instance_in, leaking_backend,
    leaking_fixture,
};
use crate::engine::{LaunchHooks, LaunchStep};
use crate::instance::record::{InstanceRecord, RecordRead};
use crate::instance::registry::registry_tests::lock_files_in;
use crate::store::layers::layers_tests::{build_expiring, days, key, names, t0};
use crate::test_support::fake_vm::{STARTED, connect_when_up};
use crate::test_support::fixtures::{ChannelProgress, MemoryProgress, anna_home};
use crate::test_support::helper;
use crate::vm::vm_tests::{queued, step_name};
use crate::{ErrorKind, Instance, InstanceState, ListFilter};

fn memory_fixture(home: &Path) -> EngineFixture {
    engine_fixture(home, Arc::new(MemoryProgress::default()))
}

fn recorded(instance: &Instance) -> Option<MachineRecord> {
    instance.dir().record().unwrap().machine
}

fn running(machine: &MachineRecord) -> bool {
    VmProcess::attach(&machine.process).unwrap().is_some()
}

/// Waits until the process of `machine` has exited (it may not have, yet, when the thing that kills it returns).
fn wait_until_dead(machine: &MachineRecord) {
    if let Some(vm) = VmProcess::attach(&machine.process).unwrap() {
        vm.wait().unwrap();
    }
}

#[skuld::test]
fn start_launches_once_and_records_the_machine(#[fixture(anna_home)] home: &Path) {
    let fx = memory_fixture(home);
    instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    instance.start().unwrap();

    assert_eq!(fx.fake.launches(), 1);
    let machine = recorded(&instance).expect("the record names the machine");
    assert_eq!(machine, fx.fake.launched()[0]);
    assert!(running(&machine));
    let infos = fx.engine.list(ListFilter::All).unwrap();
    assert_eq!(infos[0].state, InstanceState::Running);
    instance.remove().unwrap();
}

#[skuld::test]
fn a_second_start_does_not_launch_again(#[fixture(anna_home)] home: &Path) {
    let fx = memory_fixture(home);
    instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    instance.start().unwrap();
    instance.start().expect("starting a running instance is a no-op");

    assert_eq!(fx.fake.launches(), 1);
    instance.remove().unwrap();
}

#[skuld::test]
fn concurrent_starts_launch_exactly_once(#[fixture(anna_home)] home: &Path) {
    let (progress, messages) = ChannelProgress::new();
    let fx = engine_fixture(home, progress);
    let dir = instance_in(&fx, "foo", home);
    let a = Instance::open_in(&fx.engine, "foo").unwrap();
    let b = Instance::open_in(&fx.engine, "foo").unwrap();
    let holder = dir.op_exclusive().unwrap();

    std::thread::scope(|scope| {
        let first = scope.spawn(|| a.start());
        let second = scope.spawn(|| b.start());
        queued(&messages, 2);
        drop(holder);
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();
    });

    assert_eq!(fx.fake.launches(), 1);
    a.remove().unwrap();
}

#[skuld::test]
fn start_and_remove_queued_together_end_with_no_instance_and_no_process(#[fixture(anna_home)] home: &Path) {
    let (progress, messages) = ChannelProgress::new();
    let fx = engine_fixture(home, progress);
    let dir = instance_in(&fx, "foo", home);
    let starter = Instance::open_in(&fx.engine, "foo").unwrap();
    let remover = Instance::open_in(&fx.engine, "foo").unwrap();
    let holder = dir.op_exclusive().unwrap();

    let (started, removed) = std::thread::scope(|scope| {
        let a = scope.spawn(move || starter.start());
        let b = scope.spawn(move || remover.remove());
        queued(&messages, 2);
        drop(holder);
        (a.join().unwrap(), b.join().unwrap())
    });

    // Either start ran first and remove then killed its machine, or remove ran first and start found nothing.
    removed.expect("the remove always succeeds");
    if let Err(e) = started {
        assert_eq!(e.kind(), ErrorKind::NotFound, "{e}");
    }
    assert!(
        names(&fx.env.dirs.instances()).is_empty(),
        "no instance and no .dead-* tree"
    );
    assert!(lock_files_in(&fx.env.dirs).is_empty());
    for machine in fx.fake.launched() {
        wait_until_dead(&machine);
        assert!(!running(&machine));
    }
}

#[skuld::test]
fn stop_then_start_gives_a_new_process(#[fixture(anna_home)] home: &Path) {
    let fx = memory_fixture(home);
    instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();
    instance.start().unwrap();
    let first = recorded(&instance).unwrap();

    instance.stop().unwrap();
    assert!(!running(&first), "stop returns after the process has exited");
    assert!(recorded(&instance).is_none(), "a stopped instance names no machine");
    instance.stop().expect("stopping a stopped instance is Ok");

    instance.start().unwrap();
    let second = recorded(&instance).unwrap();
    assert_ne!(first.process, second.process);
    assert_eq!(fx.fake.launches(), 2);
    instance.remove().unwrap();
}

#[skuld::test]
fn remove_after_start_kills_the_machine_and_deletes_everything(#[fixture(anna_home)] home: &Path) {
    let fx = memory_fixture(home);
    instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();
    instance.start().unwrap();
    let machine = recorded(&instance).unwrap();
    let port = machine.ssh[0].addr.port();
    let vm = VmProcess::attach(&machine.process).unwrap().unwrap();
    assert!(connect_when_up(&vm, port).starts_with("up "));

    instance.remove().unwrap();

    assert!(!running(&machine));
    assert!(names(&fx.env.dirs.instances()).is_empty());
    assert!(lock_files_in(&fx.env.dirs).is_empty());
}

#[skuld::test]
fn ops_work_on_a_machine_started_by_another_process(#[fixture(anna_home)] home: &Path) {
    let fx = memory_fixture(home);
    instance_in(&fx, "foo", home);
    let output = helper::command("fake-cli-start", &[&home.display().to_string(), "foo"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let line = String::from_utf8(output.stdout).unwrap();
    let machine: MachineRecord = serde_json::from_str(line.strip_prefix("started ").unwrap()).unwrap();
    assert!(running(&machine), "the VM outlives the process that started it");

    let instance = Instance::open_in(&fx.engine, "foo").unwrap();
    assert_eq!(
        fx.engine.list(ListFilter::All).unwrap()[0].state,
        InstanceState::Running
    );
    instance.stop().unwrap();
    assert!(!running(&machine));
    assert_eq!(
        fx.engine.list(ListFilter::All).unwrap()[0].state,
        InstanceState::Stopped
    );
}

/// Runs `fake-cli-start` parked at `step`, kills it there, and returns the machine it had launched by then.
fn killed_at(fx: &EngineFixture, home: &Path, step: LaunchStep) -> (Child, MachineRecord) {
    let mut cli = helper::command("fake-cli-start", &[&home.display().to_string(), "foo", step_name(step)])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(cli.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    let machine: MachineRecord = serde_json::from_str(line.strip_prefix("parked ").expect(&line)).unwrap();
    assert!(running(&machine), "the VM exists while the CLI is parked");

    cli.kill().unwrap();
    cli.wait().unwrap();
    wait_until_dead(&machine);
    assert_eq!(fx.fake.launches(), 0, "this process launched nothing");
    (cli, machine)
}

fn a_cli_killed_at(step: LaunchStep, home: &Path) {
    let fx = memory_fixture(home);
    let dir = instance_in(&fx, "foo", home);
    let (_cli, machine) = killed_at(&fx, home, step);

    assert!(!running(&machine), "the VM died with the CLI");
    let before_the_run = !matches!(step, LaunchStep::Resumed);
    if before_the_run {
        assert!(
            !dir.path().join(STARTED).exists(),
            "a VM killed before the resume never ran"
        );
    }
    let record = dir.record().unwrap().machine;
    match step {
        LaunchStep::Spawned => assert!(record.is_none(), "nothing was recorded yet"),
        LaunchStep::Recorded => assert_eq!(record, Some(machine)),
        // The endpoints are recorded after the `Resumed` step, so a CLI killed there left the record from before.
        LaunchStep::Resumed => assert_eq!(
            record,
            Some(MachineRecord {
                ssh: Vec::new(),
                ..machine
            })
        ),
    }
    assert_eq!(
        fx.engine.list(ListFilter::All).unwrap()[0].state,
        InstanceState::Stopped
    );

    let instance = Instance::open_in(&fx.engine, "foo").unwrap();
    instance.start().expect("a later start launches again");
    assert_eq!(fx.fake.launches(), 1);
    assert!(running(&recorded(&instance).unwrap()));
    instance.remove().unwrap();
}

#[skuld::test]
fn a_cli_killed_after_the_spawn_leaves_no_vm(#[fixture(anna_home)] home: &Path) {
    a_cli_killed_at(LaunchStep::Spawned, home);
}

#[skuld::test]
fn a_cli_killed_after_the_record_leaves_no_vm(#[fixture(anna_home)] home: &Path) {
    a_cli_killed_at(LaunchStep::Recorded, home);
}

#[skuld::test]
fn a_cli_killed_after_the_resume_leaves_no_vm(#[fixture(anna_home)] home: &Path) {
    a_cli_killed_at(LaunchStep::Resumed, home);
}

#[skuld::test]
fn a_stopped_instance_on_a_superseded_generation_survives_gc_and_starts(#[fixture(anna_home)] home: &Path) {
    let fx = memory_fixture(home);
    let chain = build_expiring(&fx.env, key(5), None, Some(t0() + days(90)));
    let first = chain.tip().layer_ref();
    let guard = fx.env.store.begin_record().unwrap();
    let mut new = crate::instance::registry::registry_tests::new_instance_on(
        &chain,
        Some("foo"),
        &crate::backend::fake::FakeStorage,
    );
    new.created_from = home.to_path_buf();
    fx.engine
        .registry()
        .create(new, &guard)
        .unwrap()
        .make_ownerless(&mut || Ok(()))
        .unwrap();
    drop(guard);
    drop(chain);

    // The key is rebuilt after its first generation entered the expiry margin: the first generation is superseded and
    // only the instance refers to it.
    fx.env.clock.set(t0() + days(85));
    drop(build_expiring(&fx.env, key(5), None, Some(t0() + days(85) + days(90))));
    fx.env.store.gc(fx.engine.registry()).unwrap();
    assert!(
        fx.env.store.generation_dir(&first).is_dir(),
        "the instance keeps its generation"
    );

    let instance = Instance::open_in(&fx.engine, "foo").unwrap();
    instance.start().expect("the fake launch finds its backing layer");
    instance.remove().unwrap();
}

#[skuld::test]
fn start_tells_the_backend_the_instance_id_and_directory(#[fixture(anna_home)] home: &Path) {
    let fx = memory_fixture(home);
    let dir = instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    instance.start().unwrap();

    let specs = fx.fake.specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].id, *dir.id());
    assert_eq!(specs[0].dir, dir.path());
    assert!(specs[0].disk.starts_with(&specs[0].dir));
    assert!(specs[0].serial_log.starts_with(&specs[0].dir));
    instance.remove().unwrap();
}

// A backend whose machine learns its endpoints only when it runs: its record says so before and after `resume`.
const BEFORE_RESUME: &str = "unknown before resume";
const AFTER_RESUME: &str = "known after resume";

struct LateRecords(Arc<FakeBackend>);

impl Backend for LateRecords {
    fn launch(&self, spec: &MachineSpec) -> crate::Result<Box<dyn Machine>> {
        Ok(Box::new(LateMachine {
            inner: self.0.launch(spec)?,
            resumed: false,
        }))
    }

    fn attach(&self, rec: &MachineRecord) -> crate::Result<Box<dyn Machine>> {
        self.0.attach(rec)
    }

    fn storage(&self) -> &dyn Storage {
        self.0.storage()
    }

    fn can_save_memory(&self) -> bool {
        false
    }
}

struct LateMachine {
    inner: Box<dyn Machine>,
    resumed: bool,
}

impl Machine for LateMachine {
    fn record(&self) -> MachineRecord {
        let mut record = self.inner.record();
        record.qmp = (if self.resumed { AFTER_RESUME } else { BEFORE_RESUME }).to_owned();
        record
    }

    fn ssh_endpoints(&self) -> &[(Endpoint, std::net::SocketAddr)] {
        self.inner.ssh_endpoints()
    }

    fn serial_log(&self) -> &Path {
        self.inner.serial_log()
    }

    fn resume(&mut self) -> crate::Result<()> {
        self.inner.resume()?;
        self.resumed = true;
        Ok(())
    }

    fn release(&mut self) -> crate::Result<()> {
        self.inner.release()
    }

    fn shutdown(&mut self) -> crate::Result<()> {
        self.inner.shutdown()
    }

    fn kill(&mut self) -> crate::Result<()> {
        self.inner.kill()
    }
}

/// Notes what the instance record on disk names as the machine's `qmp` at each launch step.
struct SeesRecord {
    instance_dir: std::sync::OnceLock<std::path::PathBuf>,
    seen: std::sync::Mutex<Vec<(LaunchStep, String)>>,
}

impl LaunchHooks for SeesRecord {
    fn reached(&self, step: LaunchStep, _machine: &MachineRecord) {
        let dir = self.instance_dir.get().expect("the test set the directory");
        let qmp = match InstanceRecord::read(dir).unwrap() {
            RecordRead::Valid(record) => record.machine.map(|m| m.qmp).unwrap_or_default(),
            _ => panic!("no readable record"),
        };
        self.seen.lock().unwrap().push((step, qmp));
    }
}

#[skuld::test]
fn start_records_the_machine_again_after_it_resumed(#[fixture(anna_home)] home: &Path) {
    let hooks = Arc::new(SeesRecord {
        instance_dir: std::sync::OnceLock::new(),
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let fx = engine_fixture_with(home, Arc::new(MemoryProgress::default()), hooks.clone(), |fake| {
        Arc::new(LateRecords(fake))
    });
    let dir = instance_in(&fx, "foo", home);
    hooks.instance_dir.set(dir.path().to_path_buf()).unwrap();
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    instance.start().unwrap();

    // The first record is written while the machine is frozen; the second after it ran.
    assert_eq!(
        *hooks.seen.lock().unwrap(),
        [
            (LaunchStep::Spawned, String::new()),
            (LaunchStep::Recorded, BEFORE_RESUME.to_owned()),
            (LaunchStep::Resumed, BEFORE_RESUME.to_owned()),
        ]
    );
    assert_eq!(recorded(&instance).unwrap().qmp, AFTER_RESUME);
    instance.remove().unwrap();
}

#[skuld::test]
fn start_gives_the_seed_the_instance_id(#[fixture(anna_home)] home: &Path) {
    let fx = memory_fixture(home);
    let dir = instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    instance.start().unwrap();

    let seed = std::fs::read_to_string(dir.path().join("seed").join("seed.fake")).unwrap();
    assert!(seed.contains(&format!("instance-id={}", dir.id())), "{seed}");
    instance.remove().unwrap();
}

#[skuld::test]
fn stop_deletes_the_leftovers_of_a_running_machine(#[fixture(anna_home)] home: &Path) {
    let fx = leaking_fixture(home, Arc::new(MemoryProgress::default()));
    instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();
    instance.start().unwrap();
    let leftovers = recorded(&instance).unwrap().leftovers;
    assert!(!leftovers.is_empty());
    assert!(
        leftovers.iter().all(|path| path.exists()),
        "the running machine made them"
    );

    instance.stop().unwrap();

    assert!(!running(&fx.fake.launched()[0]));
    for leftover in leftovers {
        assert!(!leftover.exists(), "stop left {}", leftover.display());
    }
    instance.remove().unwrap();
}

#[skuld::test]
fn start_over_a_stale_record_deletes_the_old_machines_leftovers(#[fixture(anna_home)] home: &Path) {
    let fx = memory_fixture(home);
    let dir = instance_in(&fx, "foo", home);
    let sleeper = Sleeper::spawn();
    let socket = home.join("stale.sock");
    std::fs::write(&socket, b"x").unwrap();
    let stale = MachineRecord {
        process: sleeper.record(),
        qmp: String::new(),
        ssh: Vec::new(),
        serial_log: std::path::PathBuf::new(),
        leftovers: vec![socket.clone()],
    };
    sleeper.finish();
    let op = dir.op_exclusive().unwrap();
    dir.update(&op, &mut |r| r.machine = Some(stale.clone())).unwrap();
    drop(op);

    let instance = Instance::open_in(&fx.engine, "foo").unwrap();
    instance.start().unwrap();

    assert_eq!(fx.fake.launches(), 1, "the dead machine is replaced");
    assert!(!socket.exists(), "the old machine's leftovers are deleted");
    instance.remove().unwrap();
}

// Failures while launching =============================================================================================

/// Holds `record.json` open without sharing from the `step` on, so that no record read or write succeeds.
struct HoldsRecordFrom {
    step: LaunchStep,
    instance_dir: std::sync::OnceLock<std::path::PathBuf>,
    held: std::sync::Mutex<Option<std::fs::File>>,
}

impl HoldsRecordFrom {
    fn new(step: LaunchStep) -> Arc<HoldsRecordFrom> {
        Arc::new(HoldsRecordFrom {
            step,
            instance_dir: std::sync::OnceLock::new(),
            held: std::sync::Mutex::new(None),
        })
    }

    fn release(&self) {
        *self.held.lock().unwrap() = None;
    }
}

impl LaunchHooks for HoldsRecordFrom {
    fn reached(&self, step: LaunchStep, _machine: &MachineRecord) {
        if step == self.step {
            use std::os::windows::fs::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(
                    self.instance_dir
                        .get()
                        .expect("the test set the directory")
                        .join("record.json"),
                )
                .unwrap();
            *self.held.lock().unwrap() = Some(file);
        }
    }
}

/// A backend whose machines count their `kill` calls, and whose process lives on when the machine is dropped (as a
/// backend's does when its machine is not a contained child): only an explicit `kill` stops it. With `fail_resume`,
/// a machine runs after `resume` but reports a failure.
struct Spied {
    inner: Arc<FakeBackend>,
    fail_resume: bool,
    kills: Arc<AtomicUsize>,
}

impl Spied {
    fn wrap(fail_resume: bool) -> (impl FnOnce(Arc<FakeBackend>) -> Arc<dyn Backend>, Arc<AtomicUsize>) {
        let kills = Arc::new(AtomicUsize::new(0));
        let counted = kills.clone();
        (
            move |inner| {
                Arc::new(Spied {
                    inner,
                    fail_resume,
                    kills: counted,
                }) as Arc<dyn Backend>
            },
            kills,
        )
    }
}

impl Backend for Spied {
    fn launch(&self, spec: &MachineSpec) -> crate::Result<Box<dyn Machine>> {
        Ok(Box::new(SpiedMachine {
            inner: self.inner.launch(spec)?,
            fail_resume: self.fail_resume,
            kills: self.kills.clone(),
        }))
    }

    fn attach(&self, rec: &MachineRecord) -> crate::Result<Box<dyn Machine>> {
        self.inner.attach(rec)
    }

    fn storage(&self) -> &dyn Storage {
        self.inner.storage()
    }

    fn can_save_memory(&self) -> bool {
        false
    }
}

struct SpiedMachine {
    inner: Box<dyn Machine>,
    fail_resume: bool,
    kills: Arc<AtomicUsize>,
}

impl Drop for SpiedMachine {
    fn drop(&mut self) {
        // A frozen machine cannot be released and is killed by its own drop; a running one now outlives this handle.
        let _ = self.inner.release();
    }
}

impl Machine for SpiedMachine {
    fn record(&self) -> MachineRecord {
        self.inner.record()
    }

    fn ssh_endpoints(&self) -> &[(Endpoint, std::net::SocketAddr)] {
        self.inner.ssh_endpoints()
    }

    fn serial_log(&self) -> &Path {
        self.inner.serial_log()
    }

    fn resume(&mut self) -> crate::Result<()> {
        self.inner.resume()?;
        if self.fail_resume {
            return Err(crate::Error::new(
                ErrorKind::Internal,
                "the machine lost its control channel",
            ));
        }
        Ok(())
    }

    fn release(&mut self) -> crate::Result<()> {
        self.inner.release()
    }

    fn shutdown(&mut self) -> crate::Result<()> {
        self.inner.shutdown()
    }

    fn kill(&mut self) -> crate::Result<()> {
        self.kills.fetch_add(1, Ordering::SeqCst);
        self.inner.kill()
    }
}

#[skuld::test]
fn a_record_write_that_fails_after_the_spawn_kills_the_unrecorded_machine(#[fixture(anna_home)] home: &Path) {
    let hooks = HoldsRecordFrom::new(LaunchStep::Spawned);
    let (wrap, kills) = Spied::wrap(false);
    let fx = engine_fixture_with(home, Arc::new(MemoryProgress::default()), hooks.clone(), wrap);
    let dir = instance_in(&fx, "foo", home);
    hooks.instance_dir.set(dir.path().to_path_buf()).unwrap();
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    let error = instance.start().expect_err("the record cannot be written");

    hooks.release();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert_eq!(fx.fake.launches(), 1);
    assert_eq!(kills.load(Ordering::SeqCst), 1, "the unrecorded machine is killed");
    let machine = &fx.fake.launched()[0];
    wait_until_dead(machine);
    assert!(!running(machine), "an unrecorded machine must not run");
    assert!(recorded(&instance).is_none());
    assert!(dir.try_op_exclusive().unwrap().is_some(), "the op lock is released");
    instance.remove().unwrap();
}

#[skuld::test]
fn a_resume_that_fails_kills_the_machine_clears_the_record_and_deletes_its_leftovers(
    #[fixture(anna_home)] home: &Path,
) {
    let (wrap, kills) = Spied::wrap(true);
    let fx = engine_fixture_from(
        home,
        Arc::new(MemoryProgress::default()),
        Arc::new(crate::engine::NoHooks),
        leaking_backend(home),
        wrap,
    );
    let dir = instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    let error = instance.start().expect_err("the resume fails");

    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert_eq!(kills.load(Ordering::SeqCst), 1, "the machine is killed");
    let machine = fx.fake.launched()[0].clone();
    wait_until_dead(&machine);
    assert!(!running(&machine), "a machine that failed to resume must not run");
    assert!(recorded(&instance).is_none(), "the record names no machine");
    assert!(!machine.leftovers.is_empty());
    for leftover in &machine.leftovers {
        assert!(!leftover.exists(), "the failed start left {}", leftover.display());
    }
    assert!(dir.try_op_exclusive().unwrap().is_some(), "the op lock is released");
    instance.remove().unwrap();
}

#[skuld::test]
fn a_second_record_write_that_fails_kills_the_machine_and_deletes_its_leftovers(#[fixture(anna_home)] home: &Path) {
    let hooks = HoldsRecordFrom::new(LaunchStep::Resumed);
    let progress = Arc::new(MemoryProgress::default());
    let (wrap, kills) = Spied::wrap(false);
    let fx = engine_fixture_from(home, progress.clone(), hooks.clone(), leaking_backend(home), wrap);
    let dir = instance_in(&fx, "foo", home);
    hooks.instance_dir.set(dir.path().to_path_buf()).unwrap();
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    let error = instance.start().expect_err("the record cannot be written");

    hooks.release();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert_eq!(kills.load(Ordering::SeqCst), 1, "the machine is killed");
    let machine = fx.fake.launched()[0].clone();
    wait_until_dead(&machine);
    assert!(
        !running(&machine),
        "a machine whose endpoints could not be recorded must not run"
    );
    assert!(!machine.leftovers.is_empty());
    for leftover in &machine.leftovers {
        assert!(!leftover.exists(), "the failed start left {}", leftover.display());
    }
    assert!(
        progress
            .messages()
            .iter()
            .any(|m| m.contains("foo") && m.contains("record still names")),
        "the record could not be cleared either, and the warning says so: {:?}",
        progress.messages()
    );
    assert!(dir.try_op_exclusive().unwrap().is_some(), "the op lock is released");
    // The record names a dead machine, which the next command on the instance clears.
    instance.stop().unwrap();
    assert!(recorded(&instance).is_none());
    instance.remove().unwrap();
}
