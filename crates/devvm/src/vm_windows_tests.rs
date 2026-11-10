use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::access::keys::InstanceKeys;
use crate::access::{SshSession, VmExit};
use crate::backend::fake::FakeBackend;
use crate::backend::{Backend, Check, Detail, Endpoint, Machine, MachineRecord, MachineSpec, Storage};
use crate::contain::VmProcess;
use crate::contain::contain_tests::Sleeper;
use crate::engine::engine_tests::{
    EngineFixture, engine_fixture, engine_fixture_from, engine_fixture_with, engine_over, engine_with_bound, image_of,
    instance_in, leaking_backend, leaking_fixture,
};
use crate::engine::keys::read_target;
use crate::engine::{LaunchHooks, LaunchStep};
use crate::exit::NullProgress;
use crate::host::symlink_metadata_if_present;
use crate::instance::record::{InstanceRecord, RecordRead};
use crate::instance::registry::registry_tests::lock_files_in;
use crate::store::layers::layers_tests::{build_expiring, days, key, names, t0};
use crate::test_support::claimed::claimed;
use crate::test_support::fake_vm::{STARTED, answer, connect_when_up, started_port};
use crate::test_support::fixtures::{ChannelProgress, MemoryProgress, unicode_home};
use crate::test_support::helper;
use crate::test_support::host::TestHost;
use crate::test_support::sshd::FAST_KEEPALIVE;
use crate::vm::vm_tests::{queued, step_name};
use crate::{Command, ErrorKind, Guest, Instance, InstanceState, ListFilter, StartOptions};

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

/// The port the fake VM reported in its state directory belongs to the process `machine` records: it already listens,
/// and its process answers. (`machine.ssh` is the SSH server of the fixture, not the VM.)
fn assert_answers_as(machine: &MachineRecord) {
    let report = answer(started_port(machine.serial_log.parent().unwrap()));
    assert!(
        report.starts_with(&format!("up pid={} ", machine.process.pid)),
        "the endpoint of process {} is answered by another VM: {report}",
        machine.process.pid
    );
}

#[skuld::test]
fn start_launches_once_and_records_the_machine(#[fixture(unicode_home)] home: &Path) {
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
fn a_second_start_does_not_launch_again(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    instance.start().unwrap();
    instance.start().expect("starting a running instance is a no-op");

    assert_eq!(fx.fake.launches(), 1);
    instance.remove().unwrap();
}

#[skuld::test]
fn concurrent_starts_launch_exactly_once(#[fixture(unicode_home)] home: &Path) {
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
fn start_and_remove_queued_together_end_with_no_instance_and_no_process(#[fixture(unicode_home)] home: &Path) {
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
fn stop_then_start_gives_a_new_process(#[fixture(unicode_home)] home: &Path) {
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
    assert_answers_as(&second);
    instance.remove().unwrap();
}

#[skuld::test]
fn stop_then_start_keeps_the_host_key_and_the_ssh_config_valid(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    let dir = instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();
    instance.start().unwrap();
    let keys = key_bytes(dir.path());

    instance.stop().unwrap();
    instance.start().unwrap();

    assert_eq!(key_bytes(dir.path()), keys, "the guest keeps its keys across a restart");
    let conf = std::fs::read_to_string(fx.env.dirs.ssh().join("foo.conf")).unwrap();
    assert!(conf.contains(&format!("Port {}", fx.server.addr().port())), "{conf}");
    let known_hosts = std::fs::read_to_string(dir.path().join("known_hosts")).unwrap();
    let host_public = std::fs::read_to_string(dir.path().join("keys").join("host.pub")).unwrap();
    let key = host_public.split_whitespace().nth(1).unwrap();
    assert!(
        known_hosts.contains(key),
        "known_hosts names the host key: {known_hosts}"
    );
    assert_eq!(
        instance.run(&Command::new(["echo", "restarted"])).unwrap().stdout,
        b"restarted
"
    );
    instance.remove().unwrap();
}

#[skuld::test]
fn concurrent_up_and_rm_leave_every_live_instance_resolvable_in_ssh_config(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    let image = image_of(&fx.engine);
    let old = ["old0", "old1", "old2"].map(|name| image.up(name, &StartOptions::default()).unwrap());
    let reaping = std::sync::atomic::AtomicBool::new(true);

    let new = std::thread::scope(|scope| {
        // The fixture's server trusts one instance's keys at a time, so the instances are brought up one after another.
        let up = scope.spawn(|| ["new0", "new1", "new2"].map(|name| image.up(name, &StartOptions::default()).unwrap()));
        let rm = scope.spawn(move || old.map(|instance| instance.remove().unwrap()));
        let reap = scope.spawn(|| {
            while reaping.load(Ordering::SeqCst) {
                fx.engine.reap();
            }
        });
        let new = up.join().unwrap();
        rm.join().unwrap();
        // Both have finished: the reaps that ran beside them are over.
        reaping.store(false, Ordering::SeqCst);
        reap.join().unwrap();
        new
    });

    let confs = |name: &str| fx.env.dirs.ssh().join(format!("{name}.conf"));
    for instance in &new {
        assert!(
            symlink_metadata_if_present(&confs(instance.name())).unwrap().is_some(),
            "{} is live and must stay resolvable",
            instance.name()
        );
    }
    for name in ["old0", "old1", "old2"] {
        assert!(
            symlink_metadata_if_present(&confs(name)).unwrap().is_none(),
            "{name} was removed"
        );
    }
    for instance in new {
        instance.remove().unwrap();
    }
}

#[skuld::test]
fn remove_after_start_kills_the_machine_and_deletes_everything(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();
    instance.start().unwrap();
    let machine = recorded(&instance).unwrap();
    let port = started_port(machine.serial_log.parent().unwrap());
    let vm = VmProcess::attach(&machine.process).unwrap().unwrap();
    assert!(connect_when_up(&vm, port).starts_with("up "));

    instance.remove().unwrap();

    assert!(!running(&machine));
    assert!(names(&fx.env.dirs.instances()).is_empty());
    assert!(lock_files_in(&fx.env.dirs).is_empty());
}

#[skuld::test]
fn ops_work_on_a_machine_started_by_another_process(#[fixture(unicode_home)] home: &Path) {
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
    let restarted = recorded(&instance).unwrap();
    assert!(running(&restarted));
    assert_answers_as(&restarted);
    instance.remove().unwrap();
}

#[skuld::test]
fn a_cli_killed_after_the_spawn_leaves_no_vm(#[fixture(unicode_home)] home: &Path) {
    a_cli_killed_at(LaunchStep::Spawned, home);
}

#[skuld::test]
fn a_cli_killed_after_the_record_leaves_no_vm(#[fixture(unicode_home)] home: &Path) {
    a_cli_killed_at(LaunchStep::Recorded, home);
}

#[skuld::test]
fn a_cli_killed_after_the_resume_leaves_no_vm(#[fixture(unicode_home)] home: &Path) {
    a_cli_killed_at(LaunchStep::Resumed, home);
}

#[skuld::test]
fn a_stopped_instance_on_a_superseded_generation_survives_gc_and_starts(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    let chain = build_expiring(&fx.env, key(5), None, Some(t0() + days(90)));
    let first = chain.tip().layer_ref();
    let guard = fx.env.store.begin_record().unwrap();
    let mut new = crate::instance::registry::registry_tests::new_instance_on(&chain, Some("foo"), fx.fake.storage());
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
fn start_tells_the_backend_the_instance_id_and_directory(#[fixture(unicode_home)] home: &Path) {
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

    fn sweep(&self, warn: &mut dyn FnMut(String)) {
        self.0.sweep(warn);
    }

    fn checks(&self, detail: Detail) -> Vec<Check> {
        self.0.checks(detail)
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
fn start_records_the_machine_again_after_it_resumed(#[fixture(unicode_home)] home: &Path) {
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
fn start_gives_the_seed_the_instance_id(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    let dir = instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    instance.start().unwrap();

    let seed = std::fs::read_to_string(dir.path().join("seed").join("seed.fake")).unwrap();
    assert!(seed.contains(&format!("instance-id={}", dir.id())), "{seed}");
    instance.remove().unwrap();
}

#[skuld::test]
fn stop_deletes_the_leftovers_of_a_running_machine(#[fixture(unicode_home)] home: &Path) {
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
fn start_over_a_stale_record_deletes_the_old_machines_leftovers(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    let dir = instance_in(&fx, "foo", home);
    let sleeper = Sleeper::spawn();
    let socket = home.join("stale.sock");
    std::fs::write(&socket, b"x").unwrap();
    let (leftovers, claim) = claimed(vec![socket.clone()]);
    let stale = MachineRecord {
        process: sleeper.record(),
        qmp: String::new(),
        ssh: Vec::new(),
        serial_log: std::path::PathBuf::new(),
        leftovers,
        claim,
        no_reboot: false,
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

// Failures while launching ============================================================================================

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
/// a machine runs after `resume` but reports a failure. With `fail_kill`, `kill` is counted and fails without ending
/// the machine.
struct Spied {
    inner: Arc<FakeBackend>,
    fail_resume: bool,
    fail_kill: bool,
    kills: Arc<AtomicUsize>,
}

impl Spied {
    fn wrap(fail_resume: bool) -> (impl FnOnce(Arc<FakeBackend>) -> Arc<dyn Backend>, Arc<AtomicUsize>) {
        Spied::wrap_with(fail_resume, false)
    }

    fn wrap_with(
        fail_resume: bool,
        fail_kill: bool,
    ) -> (impl FnOnce(Arc<FakeBackend>) -> Arc<dyn Backend>, Arc<AtomicUsize>) {
        let kills = Arc::new(AtomicUsize::new(0));
        let counted = kills.clone();
        (
            move |inner| {
                Arc::new(Spied {
                    inner,
                    fail_resume,
                    fail_kill,
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
            fail_kill: self.fail_kill,
            kills: self.kills.clone(),
        }))
    }

    fn attach(&self, rec: &MachineRecord) -> crate::Result<Box<dyn Machine>> {
        self.inner.attach(rec)
    }

    fn storage(&self) -> &dyn Storage {
        self.inner.storage()
    }

    fn sweep(&self, warn: &mut dyn FnMut(String)) {
        self.inner.sweep(warn);
    }

    fn checks(&self, detail: Detail) -> Vec<Check> {
        self.inner.checks(detail)
    }

    fn can_save_memory(&self) -> bool {
        false
    }
}

struct SpiedMachine {
    inner: Box<dyn Machine>,
    fail_resume: bool,
    fail_kill: bool,
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
        if self.fail_kill {
            return Err(crate::Error::new(ErrorKind::Io, "the control channel refused the kill"));
        }
        self.inner.kill()
    }
}

/// A backend whose machines, when released, say whether the instance's op lock was held at that moment.
struct NotesOpLock {
    inner: Arc<FakeBackend>,
    dir: Arc<crate::instance::dir::InstanceDir>,
    held_at_release: Arc<std::sync::Mutex<Vec<bool>>>,
    fail_release: bool,
}

impl Backend for NotesOpLock {
    fn launch(&self, spec: &MachineSpec) -> crate::Result<Box<dyn Machine>> {
        Ok(Box::new(NotesOpLockMachine {
            inner: self.inner.launch(spec)?,
            dir: self.dir.clone(),
            held_at_release: self.held_at_release.clone(),
            fail_release: self.fail_release,
        }))
    }

    fn attach(&self, rec: &MachineRecord) -> crate::Result<Box<dyn Machine>> {
        self.inner.attach(rec)
    }

    fn storage(&self) -> &dyn Storage {
        self.inner.storage()
    }

    fn sweep(&self, warn: &mut dyn FnMut(String)) {
        self.inner.sweep(warn);
    }

    fn checks(&self, detail: Detail) -> Vec<Check> {
        self.inner.checks(detail)
    }

    fn can_save_memory(&self) -> bool {
        false
    }
}

struct NotesOpLockMachine {
    inner: Box<dyn Machine>,
    dir: Arc<crate::instance::dir::InstanceDir>,
    held_at_release: Arc<std::sync::Mutex<Vec<bool>>>,
    fail_release: bool,
}

impl Machine for NotesOpLockMachine {
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
        self.inner.resume()
    }

    fn release(&mut self) -> crate::Result<()> {
        let held = self.dir.try_op_exclusive().unwrap().is_none();
        self.held_at_release.lock().unwrap().push(held);
        if self.fail_release {
            return Err(crate::Error::new(ErrorKind::Internal, "the machine cannot be released"));
        }
        self.inner.release()
    }

    fn shutdown(&mut self) -> crate::Result<()> {
        self.inner.shutdown()
    }

    fn kill(&mut self) -> crate::Result<()> {
        self.inner.kill()
    }
}

#[skuld::test]
fn start_releases_the_machine_while_it_holds_the_op_lock(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    let dir = Arc::new(instance_in(&fx, "foo", home));
    let held_at_release = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine = notes_op_lock_engine(&fx, &dir, &held_at_release, false);
    let instance = Instance::open_in(&engine, "foo").unwrap();

    instance.start().unwrap();

    assert_eq!(
        *held_at_release.lock().unwrap(),
        [true],
        "another start could otherwise see a machine that is still contained and about to be released"
    );
    instance.remove().unwrap();
}

#[skuld::test]
fn a_release_that_fails_kills_the_machine_and_clears_the_record(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    let dir = Arc::new(instance_in(&fx, "foo", home));
    let held_at_release = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine = notes_op_lock_engine(&fx, &dir, &held_at_release, true);
    let instance = Instance::open_in(&engine, "foo").unwrap();

    let error = instance.start().expect_err("the release fails");

    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    let machine = fx.fake.launched()[0].clone();
    wait_until_dead(&machine);
    assert!(!running(&machine), "a machine that cannot be released must not run");
    assert!(recorded(&instance).is_none());
    assert!(dir.try_op_exclusive().unwrap().is_some(), "the op lock is released");
    instance.remove().unwrap();
}

/// An engine over the fixture's state whose backend notes the op lock at each release.
fn notes_op_lock_engine(
    fx: &EngineFixture,
    dir: &Arc<crate::instance::dir::InstanceDir>,
    held_at_release: &Arc<std::sync::Mutex<Vec<bool>>>,
    fail_release: bool,
) -> Arc<crate::engine::Engine> {
    crate::engine::Engine::with_parts(crate::engine::EngineParts {
        dirs: fx.env.dirs.clone(),
        host: &crate::test_support::host::TestHost,
        backend: Some(Arc::new(NotesOpLock {
            inner: fx.fake.clone(),
            dir: dir.clone(),
            held_at_release: held_at_release.clone(),
            fail_release,
        })),
        guests: fx.guests.clone(),
        clock: Arc::new(crate::store::layers::SystemClock),
        progress: Arc::new(MemoryProgress::default()),
        hooks: Arc::new(crate::engine::NoHooks),
        boot_bound: crate::engine::DEFAULT_BOOT_BOUND,
    })
}

#[skuld::test]
fn a_record_write_that_fails_after_the_spawn_kills_the_unrecorded_machine(#[fixture(unicode_home)] home: &Path) {
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
    #[fixture(unicode_home)] home: &Path,
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
fn a_second_record_write_that_fails_kills_the_machine_and_deletes_its_leftovers(#[fixture(unicode_home)] home: &Path) {
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

/// Ends the process of a machine that a failed kill left running, so the test does not leak it.
struct KillsOnDrop(MachineRecord);

impl Drop for KillsOnDrop {
    fn drop(&mut self) {
        if let Ok(Some(mut vm)) = VmProcess::attach(&self.0.process) {
            let _ = vm.kill();
        }
    }
}

#[skuld::test]
fn a_resume_that_fails_with_a_machine_that_cannot_be_killed_keeps_the_record_and_names_both_errors(
    #[fixture(unicode_home)] home: &Path,
) {
    let (wrap, kills) = Spied::wrap_with(true, true);
    let fx = engine_fixture_with(
        home,
        Arc::new(MemoryProgress::default()),
        Arc::new(crate::engine::NoHooks),
        wrap,
    );
    let dir = instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    let error = instance.start().expect_err("the resume fails");

    let machine = fx.fake.launched()[0].clone();
    let _leak_guard = KillsOnDrop(machine.clone());
    assert_eq!(kills.load(Ordering::SeqCst), 1, "the machine was to be killed");
    assert!(running(&machine), "the kill failed, so the machine still runs");
    let kept = recorded(&instance).expect("the record still names the machine that may run");
    assert_eq!(kept.process, machine.process);
    let text = error.to_string();
    assert!(
        text.contains("lost its control channel"),
        "the resume failure is lost: {text}"
    );
    assert!(text.contains("refused the kill"), "the kill failure is lost: {text}");
    assert!(dir.try_op_exclusive().unwrap().is_some(), "the op lock is released");
    // The next command on the instance finds the machine through the record and ends it.
    instance.stop().unwrap();
    assert!(!running(&machine));
    assert!(recorded(&instance).is_none());
    instance.remove().unwrap();
}

#[skuld::test]
fn an_unrecorded_machine_that_cannot_be_killed_is_named_in_the_error(#[fixture(unicode_home)] home: &Path) {
    let hooks = HoldsRecordFrom::new(LaunchStep::Spawned);
    let (wrap, kills) = Spied::wrap_with(false, true);
    let fx = engine_fixture_with(home, Arc::new(MemoryProgress::default()), hooks.clone(), wrap);
    let dir = instance_in(&fx, "foo", home);
    hooks.instance_dir.set(dir.path().to_path_buf()).unwrap();
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    let error = instance.start().expect_err("the record cannot be written");

    hooks.release();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert_eq!(kills.load(Ordering::SeqCst), 1);
    let text = error.to_string();
    assert!(
        text.contains("refused the kill") && text.contains("never recorded"),
        "{text}"
    );
    instance.remove().unwrap();
}

// Keys, the ssh_config entry and the wait for the guest ===============================================================

const KEY_FILES: [&str; 4] = ["client", "client.pub", "host", "host.pub"];

fn key_bytes(instance_dir: &Path) -> Vec<Vec<u8>> {
    KEY_FILES
        .iter()
        .map(|name| std::fs::read(instance_dir.join("keys").join(name)).unwrap())
        .collect()
}

/// What was true when the fake guest was asked whether it was ready.
#[derive(Debug, PartialEq, Eq)]
struct AtReady {
    seed_written: bool,
    keys_written: bool,
    conf_written: bool,
    still_contained: bool,
}

/// Records an [`AtReady`] for the instance in `instance_dir` each time the guests of `fx` are asked.
fn observe_readiness(fx: &EngineFixture, instance_dir: &Path) -> Arc<std::sync::Mutex<Vec<AtReady>>> {
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (seen, dir) = (observed.clone(), instance_dir.to_path_buf());
    let conf = fx.env.dirs.ssh().join("foo.conf");
    let is_file = |path: &Path| {
        symlink_metadata_if_present(path)
            .unwrap()
            .is_some_and(|meta| meta.is_file())
    };
    fx.guests.on_ready(move || {
        seen.lock().unwrap().push(AtReady {
            seed_written: is_file(&dir.join("seed").join("seed.fake")),
            keys_written: KEY_FILES.iter().all(|name| is_file(&dir.join("keys").join(name))),
            conf_written: is_file(&conf),
            still_contained: answer(started_port(&dir)).contains("kill-on-close=true"),
        });
    });
    observed
}

#[skuld::test]
fn instance_start_generates_keys_writes_the_ssh_config_entry_and_waits_for_readiness_before_releasing(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = memory_fixture(home);
    let dir = instance_in(&fx, "foo", home);
    let observed = observe_readiness(&fx, dir.path());
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    instance.start().unwrap();

    assert_eq!(
        *observed.lock().unwrap(),
        [AtReady {
            seed_written: true,
            keys_written: true,
            conf_written: true,
            still_contained: true,
        }],
        "the guest is asked once, after the keys, the seed and the conf, while the machine is still contained"
    );
    let machine = recorded(&instance).unwrap();
    assert!(
        answer(started_port(dir.path())).contains("kill-on-close=false"),
        "the machine is released once the guest has answered"
    );
    assert!(running(&machine));
    instance.remove().unwrap();
}

#[skuld::test]
fn the_ssh_config_entry_names_the_alias_the_key_files_and_the_guests_port(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    let dir = instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    instance.start().unwrap();

    let machine = recorded(&instance).unwrap();
    let port = machine.ssh[0].addr.port();
    assert_eq!(
        port,
        fx.server.addr().port(),
        "the machine reports the guest's SSH server"
    );
    assert_ne!(port, started_port(dir.path()), "and not the port of the VM process");
    let slashed = |path: std::path::PathBuf| path.to_str().unwrap().replace('\\', "/");
    let identity = slashed(dir.path().join("keys").join("client"));
    let known_hosts = slashed(dir.path().join("known_hosts"));
    let conf = std::fs::read_to_string(fx.env.dirs.ssh().join("foo.conf")).unwrap();
    for line in [
        "Host foo".to_owned(),
        format!("    Port {port}"),
        format!("    IdentityFile \"{identity}\""),
        format!("    UserKnownHostsFile \"{known_hosts}\""),
        format!("    HostKeyAlias devvm-{}", dir.id()),
    ] {
        assert!(
            conf.lines().any(|l| l == line),
            "{line:?} is not a line of:
{conf}"
        );
    }
    let host_public = InstanceKeys::read(&dir.path().join("keys")).unwrap().host_public;
    let mut key = host_public.split_whitespace();
    let (kind, key) = (key.next().unwrap(), key.next().unwrap());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("known_hosts")).unwrap(),
        format!(
            "devvm-{} {kind} {key}
",
            dir.id()
        ),
        "known_hosts holds the instance's host key under its alias"
    );
    instance.remove().unwrap();
}

#[skuld::test]
fn an_unready_guest_reports_what_it_awaited_and_the_serial_log(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    let dir = instance_in(&fx, "foo", home);
    // The listener is held for the whole test and never accepts: the port stays ours, and no SSH banner ever comes.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let nothing = silent.local_addr().unwrap();
    let backend = Arc::new(FakeBackend::new(&TestHost).with_ssh_endpoint(nothing));
    let engine = engine_with_bound(
        fx.env.dirs.clone(),
        &TestHost,
        Some(backend),
        fx.guests.clone(),
        Arc::new(MemoryProgress::default()),
        std::time::Duration::from_millis(300),
        Arc::new(crate::engine::NoHooks),
    );

    let error = Instance::open_in(&engine, "foo")
        .unwrap()
        .start()
        .expect_err("no guest answers");

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    let message = error.message();
    assert!(
        message.contains(&format!("waiting for SSH on {nothing} to answer")),
        "{message}"
    );
    assert!(message.contains("300ms"), "the bound is named: {message}");
    assert!(
        message.contains(&dir.path().join("serial.log").display().to_string()),
        "the serial log is named: {message}"
    );
}

#[skuld::test]
fn a_readiness_failure_kills_the_machine_and_clears_the_record(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    let dir = instance_in(&fx, "foo", home);
    fx.guests.ready_when("exit 3");
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    let error = instance.start().expect_err("the guest is not ready");

    assert!(error.message().contains("exit 3"), "{error}");
    let machine = fx.fake.launched()[0].clone();
    wait_until_dead(&machine);
    assert!(!running(&machine), "a guest that never became ready must not run");
    assert!(recorded(&instance).is_none(), "the record names no machine");
    assert!(dir.try_op_exclusive().unwrap().is_some(), "the op lock is released");
    fx.guests.ready_when("echo ready");
    instance.start().expect("the instance starts once the guest answers");
    instance.remove().unwrap();
}

#[skuld::test]
fn keys_are_generated_once_and_a_restart_reuses_them_and_the_seed_bytes(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    let dir = instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();
    let seed = || std::fs::read(dir.path().join("seed").join("seed.fake")).unwrap();

    instance.start().unwrap();
    let (keys, first_seed) = (key_bytes(dir.path()), seed());
    instance.stop().unwrap();
    instance.start().unwrap();

    assert_eq!(key_bytes(dir.path()), keys, "a restart reads the keys it made");
    assert_eq!(seed(), first_seed, "and feeds the guest the same seed");
    instance.remove().unwrap();
}

#[skuld::test]
fn the_seed_carries_the_instances_keys_and_a_restart_the_same_ones(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    let dir = instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    instance.start().unwrap();
    instance.stop().unwrap();
    instance.start().unwrap();

    let keys = InstanceKeys::read(&dir.path().join("keys")).unwrap();
    let seeds = fx.guests.seeds();
    assert_eq!(seeds.len(), 2);
    for seed in &seeds {
        assert_eq!(seed.keys.client_public, keys.client_public);
        assert_eq!(seed.keys.host_public, keys.host_public);
        assert_eq!(
            secrecy::ExposeSecret::expose_secret(&seed.keys.host_private),
            secrecy::ExposeSecret::expose_secret(&keys.host_private)
        );
        assert_eq!(seed.instance_id, *dir.id());
    }
    instance.remove().unwrap();
}

#[skuld::test]
fn a_second_process_builds_its_target_from_the_record_and_the_keys(#[fixture(unicode_home)] home: &Path) {
    let fx = memory_fixture(home);
    instance_in(&fx, "foo", home);
    Instance::open_in(&fx.engine, "foo").unwrap().start().unwrap();

    // An engine of its own over the same directories shares nothing in memory with the one that started the machine.
    let other = engine_over(
        &fx,
        &TestHost,
        Some(fx.fake.clone()),
        Arc::new(MemoryProgress::default()),
    );
    let instance = Instance::open_in(&other, "foo").unwrap();
    let op = instance.dir().op_shared().unwrap();
    let target = read_target(&other, instance.dir(), &op).unwrap().target;
    let session = SshSession::connect(&target, FAST_KEEPALIVE, VmExit::never(), None, &NullProgress).unwrap();
    let output = session.exec_collect("echo from another process").unwrap();

    assert_eq!(output.stdout, b"from another process\n");
    drop((session, op));
    instance.remove().unwrap();
}

#[skuld::test]
fn key_write_warnings_are_shown_as_progress_lines(#[fixture(unicode_home)] home: &Path) {
    let progress = Arc::new(MemoryProgress::default());
    let fx = engine_fixture(home, progress.clone());
    let dir = instance_in(&fx, "foo", home);
    // A file where a temporary directory of an earlier write should be: it cannot be removed as one.
    std::fs::write(dir.path().join(".keys-stuck"), "not a directory").unwrap();
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    instance.start().unwrap();

    let messages = progress.messages();
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("instance foo: ") && m.contains(".keys-stuck")),
        "{messages:?}"
    );
    instance.remove().unwrap();
}

#[skuld::test]
fn an_ssh_config_refusal_is_shown_and_the_instance_still_starts(#[fixture(unicode_home)] home: &Path) {
    // `%` in the state directory: ssh cannot include a file from it, so `ensure_top_level` refuses.
    let home = home.join("100%");
    std::fs::create_dir(&home).unwrap();
    let progress = Arc::new(MemoryProgress::default());
    let fx = engine_fixture(&home, progress.clone());
    instance_in(&fx, "foo", &home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    instance
        .start()
        .expect("the embedded client does not read the ssh configuration");

    let messages = progress.messages();
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("instance foo: cannot write the ssh configuration:") && m.contains('%')),
        "{messages:?}"
    );
    assert!(running(&recorded(&instance).unwrap()));
    instance.remove().unwrap();
}

#[skuld::test]
fn an_ssh_config_refusal_and_a_failed_entry_write_are_both_shown(#[fixture(unicode_home)] home: &Path) {
    // `%` in the state directory: `ensure_top_level` refuses. A directory where `foo.conf` goes: the entry cannot be
    // written either.
    let home = home.join("100%");
    std::fs::create_dir(&home).unwrap();
    let progress = Arc::new(MemoryProgress::default());
    let fx = engine_fixture(&home, progress.clone());
    instance_in(&fx, "foo", &home);
    std::fs::create_dir_all(fx.env.dirs.ssh().join("foo.conf")).unwrap();
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    instance
        .start()
        .expect("the embedded client does not read the ssh configuration");

    let messages = progress.messages();
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("instance foo: cannot write the ssh configuration:")
                && m.find("ssh cannot include portably") < m.find("foo.conf")
                && m.contains("ssh cannot include portably")
                && m.ends_with(" as well")),
        "{messages:?}"
    );
    instance.remove().unwrap();
}

#[skuld::test]
fn a_degraded_first_boot_is_reported_on_the_progress_channel_and_the_start_succeeds(
    #[fixture(unicode_home)] home: &Path,
) {
    let progress = Arc::new(MemoryProgress::default());
    let fx = engine_fixture(home, progress.clone());
    instance_in(&fx, "foo", home);
    fx.guests.degraded("degraded done: modules failed");
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    instance.start().unwrap();

    let messages = progress.messages();
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("instance foo: ") && m.contains("degraded done: modules failed")),
        "{messages:?}"
    );
    instance.remove().unwrap();
}
