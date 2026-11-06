use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::Arc;

use crate::backend::MachineRecord;
use crate::contain::VmProcess;
use crate::engine::engine_tests::{EngineFixture, engine_fixture, image_of, leaking_fixture};
use crate::instance::name::InstanceName;
use crate::instance::record::{MarkerRead, Owner, read_marker};
use crate::instance::registry::registry_tests::lock_files_in;
use crate::store::layers::layers_tests::names;
use crate::test_support::fake_vm::answer;
use crate::test_support::fake_vm::{STARTED, connect_when_up, started_port};
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::helper;
use crate::{Command, ErrorKind, Guest, Instance, InstanceState, ListFilter, StartOptions};

fn memory_fixture(home: &Path) -> (EngineFixture, Arc<MemoryProgress>) {
    let progress = Arc::new(MemoryProgress::default());
    (engine_fixture(home, progress.clone()), progress)
}

fn running(machine: &MachineRecord) -> bool {
    VmProcess::attach(&machine.process).unwrap().is_some()
}

fn wait_until_dead(machine: &MachineRecord) {
    if let Some(vm) = VmProcess::attach(&machine.process).unwrap() {
        vm.wait().unwrap();
    }
}

fn nothing_is_left(fx: &EngineFixture) {
    assert!(names(&fx.env.dirs.instances()).is_empty());
    assert!(lock_files_in(&fx.env.dirs).is_empty());
}

#[skuld::test]
fn image_start_makes_an_owned_instance_with_a_running_vm(#[fixture(unicode_home)] home: &Path) {
    let (fx, _) = memory_fixture(home);
    let vm = image_of(&fx.engine).start(&StartOptions::default()).unwrap();

    let infos = fx.engine.list(ListFilter::All).unwrap();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].name, vm.name());
    assert_eq!(infos[0].state, InstanceState::Running);
    assert!(!infos[0].ownerless);
    let dir = vm.instance().dir();
    assert!(matches!(dir.marker().unwrap(), MarkerRead::Valid(m) if m.owner == Owner::Process));
    let machine = dir.record().unwrap().machine.expect("the record names the machine");
    assert!(running(&machine));
    assert_eq!(fx.fake.launches(), 1);
}

#[skuld::test]
fn vm_drop_kills_the_machine_and_removes_the_instance(#[fixture(unicode_home)] home: &Path) {
    let (fx, _) = memory_fixture(home);
    let vm = image_of(&fx.engine).start(&StartOptions::default()).unwrap();
    let machine = vm.instance().dir().record().unwrap().machine.unwrap();

    drop(vm);

    assert!(!running(&machine), "drop returns after the VM has exited");
    nothing_is_left(&fx);
}

#[skuld::test]
fn vm_drop_deletes_the_leftovers_of_the_machine(#[fixture(unicode_home)] home: &Path) {
    let fx = leaking_fixture(home, Arc::new(MemoryProgress::default()));
    let vm = image_of(&fx.engine).start(&StartOptions::default()).unwrap();
    let leftovers = vm.instance().dir().record().unwrap().machine.unwrap().leftovers;
    assert!(!leftovers.is_empty() && leftovers.iter().all(|path| path.exists()));

    drop(vm);

    for leftover in leftovers {
        assert!(!leftover.exists(), "drop left {}", leftover.display());
    }
    nothing_is_left(&fx);
}

#[skuld::test]
fn vm_drop_during_a_panic_unwinds_and_destroys(#[fixture(unicode_home)] home: &Path) {
    let (fx, _) = memory_fixture(home);
    let image = image_of(&fx.engine);

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _vm = image.start(&StartOptions::default()).unwrap();
        panic!("the test body fails while the Vm lives");
    }));

    assert!(unwound.is_err());
    for machine in fx.fake.launched() {
        assert!(!running(&machine));
    }
    nothing_is_left(&fx);
}

#[skuld::test]
fn vm_drop_never_fails_when_the_instance_is_busy_and_the_next_reap_finishes_it(#[fixture(unicode_home)] home: &Path) {
    let (fx, progress) = memory_fixture(home);
    let vm = image_of(&fx.engine).start(&StartOptions::default()).unwrap();
    let name = vm.name().to_owned();
    let machine = vm.instance().dir().record().unwrap().machine.unwrap();
    let other = Instance::open_in(&fx.engine, &name).unwrap();
    let busy = other.dir().op_exclusive().unwrap();

    drop(vm);

    assert!(!running(&machine), "the VM is dead even though the instance is busy");
    assert!(
        progress.messages().iter().any(|m| m.contains(&name)),
        "the warning names the instance: {:?}",
        progress.messages()
    );
    assert!(
        fx.env.dirs.instances().join(&name).exists(),
        "the busy instance is left"
    );
    drop(busy);
    drop(other);
    fx.engine.reap();
    nothing_is_left(&fx);
}

/// Starts helper `fake-cli-run`, which owns a `DiesWithOwner` VM, and returns it with the machine's record.
fn cli_run(home: &Path) -> (Child, String, MachineRecord) {
    let mut cli = helper::command("fake-cli-run", &[&home.display().to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(cli.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    let (name, machine) = line
        .strip_prefix("running ")
        .and_then(|rest| rest.split_once(' '))
        .unwrap_or_else(|| panic!("unexpected output {line:?}"));
    let machine = serde_json::from_str(machine).unwrap();
    (cli, name.to_owned(), machine)
}

#[skuld::test]
fn an_owner_killed_while_its_vm_runs_is_reaped_with_the_vm(#[fixture(unicode_home)] home: &Path) {
    let (fx, _) = memory_fixture(home);
    let (mut cli, name, machine) = cli_run(home);
    assert!(running(&machine));
    assert!(fx.env.dirs.instances().join(&name).exists());

    cli.kill().unwrap();
    cli.wait().unwrap();
    wait_until_dead(&machine);
    fx.engine.reap();

    nothing_is_left(&fx);
}

/// Runs `fake-cli-up` parked at `step`, kills it there, and returns the machine it had launched.
fn up_killed_at(home: &Path, step: &str) -> MachineRecord {
    let mut cli = helper::command("fake-cli-up", &[&home.display().to_string(), step])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(cli.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    let machine: MachineRecord = serde_json::from_str(line.strip_prefix("parked ").expect(&line)).unwrap();
    assert!(running(&machine));
    cli.kill().unwrap();
    cli.wait().unwrap();
    machine
}

fn an_up_killed_at(step: &str, home: &Path) {
    let (fx, _) = memory_fixture(home);
    let machine = up_killed_at(home, step);
    let placed = names(&fx.env.dirs.instances());
    assert_eq!(placed.len(), 1, "{placed:?}");
    let started_marker = fx.env.dirs.instances().join(&placed[0]).join(STARTED);
    if step == "disarmed" {
        // The containment was disarmed, so the VM outlives the killed CLI until the reaper finds it.
        assert!(running(&machine), "a disarmed VM survives its owner");
    } else {
        wait_until_dead(&machine);
    }

    if step == "spawned" || step == "recorded" {
        assert!(!started_marker.exists(), "a VM killed before the resume never ran");
    }

    fx.engine.reap();

    wait_until_dead(&machine);
    assert!(!running(&machine), "the reaper kills a VM the owner left behind");
    nothing_is_left(&fx);
}

#[skuld::test]
fn an_up_killed_after_the_spawn_is_reaped_with_its_vm(#[fixture(unicode_home)] home: &Path) {
    an_up_killed_at("spawned", home);
}

#[skuld::test]
fn an_up_killed_after_the_record_is_reaped_with_its_vm(#[fixture(unicode_home)] home: &Path) {
    an_up_killed_at("recorded", home);
}

#[skuld::test]
fn an_up_killed_after_the_resume_is_reaped_with_its_vm(#[fixture(unicode_home)] home: &Path) {
    an_up_killed_at("resumed", home);
}

#[skuld::test]
fn an_up_killed_after_the_disarm_is_reaped_with_its_vm(#[fixture(unicode_home)] home: &Path) {
    an_up_killed_at("disarmed", home);
}

#[skuld::test]
fn an_ownerless_instance_survives_the_cli_and_only_remove_deletes_it(#[fixture(unicode_home)] home: &Path) {
    let (fx, _) = memory_fixture(home);
    // The helper reads `kill-on-close=false` from the VM before it exits, so survival does not rest on timing.
    let output = helper::command("fake-cli-up", &[&home.display().to_string()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let line = String::from_utf8(output.stdout).unwrap();
    let (name, machine) = line
        .strip_prefix("up ")
        .and_then(|rest| rest.trim_end().split_once(' '))
        .unwrap_or_else(|| panic!("unexpected output {line:?}"));
    let machine: MachineRecord = serde_json::from_str(machine).unwrap();

    fx.engine.reap();
    assert!(
        fx.env.dirs.instances().join(name).exists(),
        "reap leaves an ownerless instance"
    );
    let vm = VmProcess::attach(&machine.process)
        .unwrap()
        .expect("the VM outlived its CLI");
    assert!(connect_when_up(&vm, started_port(machine.serial_log.parent().unwrap())).contains("kill-on-close=false"));
    let infos = fx.engine.list(ListFilter::All).unwrap();
    assert!(infos[0].ownerless);
    assert_eq!(infos[0].state, InstanceState::Running);

    Instance::open_in(&fx.engine, name).unwrap().remove().unwrap();

    assert!(!running(&machine));
    nothing_is_left(&fx);
    assert_eq!(
        fx.engine
            .registry()
            .open(&InstanceName::parse(name).unwrap())
            .err()
            .unwrap()
            .kind(),
        ErrorKind::NotFound
    );
}

#[skuld::test]
fn a_vm_that_dies_with_its_owner_cannot_be_handed_over(#[fixture(unicode_home)] home: &Path) {
    let (fx, _) = memory_fixture(home);
    let vm = image_of(&fx.engine).start(&StartOptions::default()).unwrap();
    let machine = vm.instance().dir().record().unwrap().machine.unwrap();

    let error = vm.hand_over(&mut || {}).err().expect("release is refused");

    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(error.message().contains("DiesWithOwner"), "{}", error.message());
    fx.engine.reap();
    wait_until_dead(&machine);
    assert!(!running(&machine), "the VM was never released");
    nothing_is_left(&fx);
}

#[skuld::test]
fn vm_guest_methods_are_not_supported_until_the_access_layer(#[fixture(unicode_home)] home: &Path) {
    let (fx, _) = memory_fixture(home);
    let vm = image_of(&fx.engine).start(&StartOptions::default()).unwrap();

    let error = vm.run(&Command::new(["true"])).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::NotSupported);
    assert_eq!(vm.reboot().unwrap_err().kind(), ErrorKind::NotSupported);
}

// `up` and the wait for the guest =====================================================================================

#[skuld::test]
fn up_hands_over_only_after_readiness(#[fixture(unicode_home)] home: &Path) {
    let (fx, _) = memory_fixture(home);
    let path = fx.env.dirs.instances().join("foo");
    let at_ready = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = at_ready.clone();
    let dir = path.clone();
    fx.guests.on_ready(move || {
        let owner = match read_marker(&dir).unwrap() {
            MarkerRead::Valid(marker) => marker.owner,
            _ => panic!("owner.json should be valid"),
        };
        let contained = answer(started_port(&dir)).contains("kill-on-close=true");
        seen.lock().unwrap().push((owner, contained));
    });

    let instance = image_of(&fx.engine).up("foo", &StartOptions::default()).unwrap();

    assert_eq!(
        *at_ready.lock().unwrap(),
        [(Owner::Process, true)],
        "the guest is asked while the process still owns the contained VM"
    );
    assert!(matches!(instance.dir().marker().unwrap(), MarkerRead::Valid(m) if m.owner == Owner::None));
    let machine = instance
        .dir()
        .record()
        .unwrap()
        .machine
        .expect("the record names the machine");
    assert!(running(&machine));
    assert!(answer(started_port(&path)).contains("kill-on-close=false"));
    instance.remove().unwrap();
    nothing_is_left(&fx);
}

#[skuld::test]
fn a_vm_that_never_gets_ready_is_killed_and_its_instance_destroyed(#[fixture(unicode_home)] home: &Path) {
    let (fx, _) = memory_fixture(home);
    fx.guests.ready_when("exit 3");

    for error in [
        image_of(&fx.engine).start(&StartOptions::default()).err().unwrap(),
        image_of(&fx.engine).up("foo", &StartOptions::default()).err().unwrap(),
    ] {
        assert!(error.message().contains("exit 3"), "{error}");
    }

    for machine in fx.fake.launched() {
        wait_until_dead(&machine);
        assert!(!running(&machine), "a guest that never became ready must not run");
    }
    assert_eq!(fx.fake.launches(), 2);
    nothing_is_left(&fx);
}
