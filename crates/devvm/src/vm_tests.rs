use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use crate::backend::MachineRecord;
use crate::backend::fake::FakeBackend;
use crate::contain::Lifetime;
use crate::contain::VmProcess;
use crate::contain::contain_tests::Sleeper;
use crate::engine::engine_tests::{engine_fixture, image_of, instance_in};
use crate::engine::{Engine, EngineParts, LaunchHooks, LaunchStep};
use crate::guest::fake::FakeGuests;
use crate::instance::name::InstanceName;
use crate::instance::record::InstanceRecord;
use crate::instance::registry::registry_tests::lock_files_in;
use crate::store::layers::layers_tests::names;
use crate::test_support::fake_vm::connect_when_up;
use crate::test_support::fixtures::{ChannelProgress, MemoryProgress, anna_home};
use crate::test_support::host::TestHost;
use crate::{Dirs, ErrorKind, Instance, StartOptions};

/// Reads progress messages until `count` of them say `waiting for`: that many commands are provably queued on a lock.
pub(crate) fn queued(messages: &Receiver<String>, count: usize) {
    let mut seen = 0;
    while seen < count {
        if messages.recv().unwrap().starts_with("waiting for") {
            seen += 1;
        }
    }
}

#[skuld::test]
fn remove_kills_a_running_machine_by_recorded_identity_and_deletes_everything(#[fixture(anna_home)] home: &Path) {
    let fx = engine_fixture(home, std::sync::Arc::new(MemoryProgress::default()));
    let dir = instance_in(&fx, "foo", home);
    let sleeper = Sleeper::spawn();
    let machine = MachineRecord {
        process: sleeper.record(),
        qmp: String::new(),
        ssh: Vec::new(),
    };
    let op = dir.op_exclusive().unwrap();
    dir.update(&op, &mut |r: &mut InstanceRecord| r.machine = Some(machine.clone()))
        .unwrap();
    drop(op);

    Instance::open_in(&fx.engine, "foo").unwrap().remove().unwrap();
    assert!(
        VmProcess::attach(&machine.process).unwrap().is_none(),
        "the VM is dead when remove returns"
    );
    assert!(names(&fx.env.dirs.instances()).is_empty());
    assert!(lock_files_in(&fx.env.dirs).is_empty());
    assert_eq!(
        Instance::open_in(&fx.engine, "foo").err().unwrap().kind(),
        ErrorKind::NotFound
    );
}

#[skuld::test]
fn remove_and_remove_queued_together_have_one_winner(#[fixture(anna_home)] home: &Path) {
    let (progress, messages) = ChannelProgress::new();
    let fx = engine_fixture(home, progress);
    let dir = instance_in(&fx, "foo", home);
    let first = Instance::open_in(&fx.engine, "foo").unwrap();
    let second = Instance::open_in(&fx.engine, "foo").unwrap();
    let holder = dir.op_exclusive().unwrap();

    let results = std::thread::scope(|scope| {
        let a = scope.spawn(move || first.remove());
        let b = scope.spawn(move || second.remove());
        // Both removals are now waiting on the op lock; without it neither would have said so.
        queued(&messages, 2);
        drop(holder);
        [a.join().unwrap(), b.join().unwrap()]
    });
    let ok = results.iter().filter(|r| r.is_ok()).count();
    let not_found = results
        .iter()
        .filter(|r| r.as_ref().is_err_and(|e| e.kind() == ErrorKind::NotFound))
        .count();
    assert_eq!((ok, not_found), (1, 1), "{results:?}");
    assert!(names(&fx.env.dirs.instances()).is_empty());
    assert!(lock_files_in(&fx.env.dirs).is_empty());
}

#[skuld::test]
fn a_remove_queued_behind_a_holder_does_not_remove_a_recreated_name(#[fixture(anna_home)] home: &Path) {
    let (progress, messages) = ChannelProgress::new();
    let fx = engine_fixture(home, progress);
    let first = instance_in(&fx, "foo", home);
    let waiting = Instance::open_in(&fx.engine, "foo").unwrap();
    let holder = first.op_exclusive().unwrap();

    std::thread::scope(|scope| {
        let waiter = scope.spawn(move || waiting.remove());
        queued(&messages, 1);

        // The instance is removed behind the holder's back and its name is taken by a new one.
        std::fs::remove_dir_all(fx.env.dirs.instances().join("foo")).unwrap();
        fx.engine.registry().locks().remove(first.id()).unwrap();
        let second = instance_in(&fx, "foo", home);
        let before = std::fs::read(second.path().join("record.json")).unwrap();
        drop(holder);

        let error = waiter.join().unwrap().expect_err("the instance it queued for is gone");
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(std::fs::read(second.path().join("record.json")).unwrap(), before);
        assert!(fx.engine.registry().open(&InstanceName::parse("foo").unwrap()).is_ok());
    });
}

#[skuld::test]
fn a_start_queued_behind_a_remove_does_not_start_a_recreated_name(#[fixture(anna_home)] home: &Path) {
    let (progress, messages) = ChannelProgress::new();
    let fx = engine_fixture(home, progress);
    let first = instance_in(&fx, "foo", home);
    let waiting = Instance::open_in(&fx.engine, "foo").unwrap();
    let holder = first.op_exclusive().unwrap();

    std::thread::scope(|scope| {
        let waiter = scope.spawn(move || waiting.start());
        queued(&messages, 1);

        std::fs::remove_dir_all(fx.env.dirs.instances().join("foo")).unwrap();
        fx.engine.registry().locks().remove(first.id()).unwrap();
        let second = instance_in(&fx, "foo", home);
        let before = std::fs::read(second.path().join("record.json")).unwrap();
        drop(holder);

        let error = waiter.join().unwrap().expect_err("the instance it queued for is gone");
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(fx.fake.launches(), 0, "nothing was launched");
        assert_eq!(std::fs::read(second.path().join("record.json")).unwrap(), before);
    });
}

#[skuld::test]
fn stop_is_idempotent(#[fixture(anna_home)] home: &Path) {
    let fx = engine_fixture(home, Arc::new(MemoryProgress::default()));
    instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();
    instance.stop().expect("a stopped instance stops");
    instance.stop().expect("and again");
}

#[skuld::test]
fn stop_and_remove_queued_together_end_with_nothing_left(#[fixture(anna_home)] home: &Path) {
    let (progress, messages) = ChannelProgress::new();
    let fx = engine_fixture(home, progress);
    let dir = instance_in(&fx, "foo", home);
    let stopper = Instance::open_in(&fx.engine, "foo").unwrap();
    let remover = Instance::open_in(&fx.engine, "foo").unwrap();
    let holder = dir.op_exclusive().unwrap();

    let (stopped, removed) = std::thread::scope(|scope| {
        let a = scope.spawn(move || stopper.stop());
        let b = scope.spawn(move || remover.remove());
        queued(&messages, 2);
        drop(holder);
        (a.join().unwrap(), b.join().unwrap())
    });
    removed.expect("the remove always succeeds");
    // Either stop ran first (a no-op on a stopped instance) or the instance was already gone.
    if let Err(e) = stopped {
        assert_eq!(e.kind(), ErrorKind::NotFound, "{e}");
    }
    assert!(names(&fx.env.dirs.instances()).is_empty());
    assert!(lock_files_in(&fx.env.dirs).is_empty());
}

// Helper processes ====================================================================================================

/// Parks the launch of `fake-cli-start` at one step: prints the machine's record and waits for stdin to close, which
/// only the test killing the process does.
struct ParkAt(LaunchStep);

impl LaunchHooks for ParkAt {
    fn reached(&self, step: LaunchStep, machine: &MachineRecord) {
        if step == self.0 {
            park(machine);
        }
    }
}

/// Prints `parked <machine record>` and blocks until stdin closes, which only a test killing the process does.
fn park(machine: &MachineRecord) {
    let mut stdout = std::io::stdout();
    writeln!(stdout, "parked {}", serde_json::to_string(machine).unwrap())
        .and_then(|()| stdout.flush())
        .unwrap();
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
}

pub(crate) fn step_name(step: LaunchStep) -> &'static str {
    match step {
        LaunchStep::Spawned => "spawned",
        LaunchStep::Recorded => "recorded",
        LaunchStep::Resumed => "resumed",
    }
}

fn step_named(name: &str) -> Option<LaunchStep> {
    [LaunchStep::Spawned, LaunchStep::Recorded, LaunchStep::Resumed]
        .into_iter()
        .find(|step| step_name(*step) == name)
}

/// The engine of a helper process: the fake backend over the state under `home`.
pub(crate) fn helper_engine(home: &Path, hooks: Arc<dyn LaunchHooks>) -> Arc<Engine> {
    Engine::with_parts(EngineParts {
        dirs: Dirs::under(home),
        host: &TestHost,
        backend: Some(Arc::new(FakeBackend::new(&TestHost))),
        guests: Arc::new(FakeGuests),
        clock: Arc::new(crate::store::layers::SystemClock),
        progress: Arc::new(crate::exit::StderrProgress),
        hooks,
    })
}

/// Helper `fake-cli-start <home> <name> [step]`: starts the instance and prints `started <machine record>`; with a
/// step it parks there instead (see [`ParkAt`]).
pub(crate) fn fake_cli_start(args: &[String]) {
    let hooks: Arc<dyn LaunchHooks> = match args.get(2) {
        Some(step) => Arc::new(ParkAt(
            step_named(step).unwrap_or_else(|| panic!("no launch step called {step}")),
        )),
        None => Arc::new(crate::engine::NoHooks),
    };
    let engine = helper_engine(Path::new(&args[0]), hooks);
    let instance = Instance::open_in(&engine, &args[1]).expect("the instance exists");
    instance.start().expect("the instance should start");
    let machine = instance
        .dir()
        .record()
        .unwrap()
        .machine
        .expect("a started instance names its machine");
    println!("started {}", serde_json::to_string(&machine).unwrap());
}

/// Helper `fake-cli-run <home>`: starts an owned VM that dies with this process, prints `running <name> <machine
/// record>` and blocks until stdin closes (that is, until the test kills it).
pub(crate) fn fake_cli_run(args: &[String]) {
    let engine = helper_engine(Path::new(&args[0]), Arc::new(crate::engine::NoHooks));
    let vm = image_of(&engine)
        .start(&StartOptions::default())
        .expect("the VM should start");
    let machine = vm
        .instance()
        .dir()
        .record()
        .unwrap()
        .machine
        .expect("a started VM names its machine");
    println!("running {} {}", vm.name(), serde_json::to_string(&machine).unwrap());
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
}

/// Helper `fake-cli-up <home> [step]`: `up` over the fake backend. An owned VM that outlives its owner, then the
/// handover. With a step (`spawned`, `recorded`, `resumed`, or `disarmed`, which is after the containment is disarmed
/// and before the owner marker is flipped) it parks there. Otherwise it reads the VM's own report of its containment
/// (it must be disarmed) and prints `up <name> <machine record>`.
pub(crate) fn fake_cli_up(args: &[String]) {
    let step = args.get(1).map(String::as_str);
    let hooks: Arc<dyn LaunchHooks> = match step.and_then(step_named) {
        Some(step) => Arc::new(ParkAt(step)),
        None => Arc::new(crate::engine::NoHooks),
    };
    let engine = helper_engine(Path::new(&args[0]), hooks);
    let vm = image_of(&engine)
        .start_vm(&StartOptions::default(), Lifetime::OutlivesOwner)
        .expect("the VM should start");
    let machine = vm
        .instance()
        .dir()
        .record()
        .unwrap()
        .machine
        .expect("a started VM names its machine");
    let instance = vm
        .hand_over(&mut || {
            if step == Some("disarmed") {
                park(&machine);
            }
        })
        .expect("the handover should succeed");

    let process = VmProcess::attach(&machine.process).unwrap().expect("the VM runs");
    let report = connect_when_up(&process, machine.ssh[0].addr.port());
    assert!(
        report.contains("kill-on-close=false"),
        "the VM is still contained: {report}"
    );
    println!("up {} {}", instance.name(), serde_json::to_string(&machine).unwrap());
}
