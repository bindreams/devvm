use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};

use crate::access::{Keepalive, SshSession, VmExit, runtime};
use crate::backend::{Backend, Machine};
use crate::engine::engine_tests::{
    EngineFixture, dead_machine, engine_fixture, image_of, image_with, is_there, stub_fixture, write_conf,
};
use crate::engine::keys::read_target;
use crate::instance::record::{MarkerRead, Owner, read_marker};
use crate::recipe::Resources;
use crate::store::layers::layers_tests::names;
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::stub_backend::{Attach, StubBackend};
use crate::vm::ops::ops_tests::{running_instance, short_bound_instance};
use crate::vm::{Guest, Handle, ops};
use crate::{Command, ErrorKind, Image, Instance, InstanceState, ListFilter, StartOptions, Vm, availability};
use skuld::env;

fn memory() -> Arc<MemoryProgress> {
    Arc::new(MemoryProgress::default())
}

#[skuld::test]
fn image_create_leaves_a_stopped_ownerless_instance(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let image = image_of(&fx.engine);
    let opts = StartOptions {
        cpus: Some(3),
        memory: Some(1 << 30),
        ..StartOptions::default()
    };

    let instance = image.create("Foo", &opts).unwrap();

    assert_eq!(instance.name(), "foo");
    assert_eq!(fx.fake.launches(), 0, "create does not start");
    let infos = fx.engine.list(ListFilter::All).unwrap();
    assert_eq!(infos.len(), 1);
    assert!(infos[0].ownerless, "an instance made by create stays until rm");
    assert_eq!(infos[0].state, InstanceState::Stopped);
    let record = instance.dir().record().unwrap();
    assert_eq!((record.cpus, record.memory), (3, 1 << 30));
    assert_eq!(record.created_from, std::env::current_dir().unwrap());
    assert!(record.machine.is_none());
    assert_eq!(
        image.create("foo", &opts).err().unwrap().kind(),
        ErrorKind::AlreadyExists
    );
}

#[skuld::test]
fn up_with_an_invalid_name_creates_nothing_and_launches_nothing(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());

    for text in ["../x", "con", "a*", ""] {
        let error = image_of(&fx.engine).up(text, &StartOptions::default()).err().unwrap();
        assert_eq!(error.kind(), ErrorKind::InvalidName, "{text:?}");
    }

    assert_eq!(fx.fake.launches(), 0);
    assert!(names(&fx.env.dirs.instances()).is_empty());
}

/// A recipe without `resources.disk` reaches the storage as no size, so the storage picks the default; a recipe with
/// one reaches it as that size.
#[skuld::test]
fn image_create_hands_the_storage_the_recipes_disk_size_or_none(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());

    for (name, disk) in [("unset", None), ("set", Some(20 << 30))] {
        let resources = Resources {
            disk,
            ..Resources::default()
        };
        image_with(&fx.engine, resources)
            .create(name, &StartOptions::default())
            .unwrap();
    }

    assert_eq!(fx.fake.disk_sizes(), [None, Some(20 << 30)]);
}

#[skuld::test]
fn a_bad_instance_name_is_rejected_before_anything_is_created(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let image = image_of(&fx.engine);

    for text in ["../x", "con", ""] {
        let error = image.create(text, &StartOptions::default()).err().unwrap();
        assert_eq!(error.kind(), ErrorKind::InvalidName, "{text:?}");
    }
    assert!(!fx.env.dirs.state.exists());
}

#[skuld::test]
fn image_vm_and_instance_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Image>();
    assert_send_sync::<Vm>();
    assert_send_sync::<Instance>();
}

#[skuld::test]
fn availability_of_an_engine_that_cannot_be_built_says_why(
    #[fixture(env)] env: &skuld::EnvGuard,
    #[fixture(unicode_home)] home: &Path,
) {
    for (variable, directory) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "run"),
    ] {
        env.set(variable, home.join(directory).to_str().unwrap());
    }
    std::fs::create_dir_all(home.join("config").join("devvm")).unwrap();
    let file = home.join("config").join("devvm").join("config.yaml");
    std::fs::write(&file, "qemu: [not, a, mapping]").unwrap();

    let unavailable = availability().expect_err("an unreadable config.yaml means no engine");

    assert!(
        unavailable.missing.contains(&file.display().to_string()),
        "{unavailable}"
    );
}

/// A `Vm` that owns a new instance on the stub backend's fixture and holds a machine the stub found.
fn vm_with_machine(fx: &EngineFixture, machine: Option<Box<dyn Machine>>) -> Vm {
    let image = image_of(&fx.engine);
    let owned = image.create_owned(None, &StartOptions::default()).unwrap();
    Vm {
        instance: Some(Instance {
            engine: fx.engine.clone(),
            handle: Handle::Owned(owned),
        }),
        machine: Mutex::new(machine),
        session: Mutex::new(None),
    }
}

fn stub_machine(stub: &StubBackend) -> Box<dyn Machine> {
    stub.attach(&dead_machine(Vec::new())).unwrap()
}

#[skuld::test]
fn a_vm_without_a_machine_cannot_be_handed_over(#[fixture(unicode_home)] home: &Path) {
    let (fx, _, _) = stub_fixture(home, Attach::Running);
    let vm = vm_with_machine(&fx, None);

    let error = vm.hand_over(&mut || {}).err().expect("nothing to hand over");

    assert_eq!(error.kind(), ErrorKind::Internal);
    assert!(error.message().contains("already handed over"), "{}", error.message());
}

#[skuld::test]
fn a_vm_whose_instance_is_not_owned_cannot_be_handed_over(#[fixture(unicode_home)] home: &Path) {
    let (fx, stub, _) = stub_fixture(home, Attach::Running);
    let instance = image_of(&fx.engine).create("foo", &StartOptions::default()).unwrap();
    let vm = Vm {
        instance: Some(instance),
        machine: Mutex::new(Some(stub_machine(&stub))),
        session: Mutex::new(None),
    };

    let error = vm
        .hand_over(&mut || {})
        .err()
        .expect("an opened instance has no owner to hand over");

    assert_eq!(error.kind(), ErrorKind::Internal);
    assert!(error.message().contains("not owned"), "{}", error.message());
}

#[skuld::test]
fn a_handover_runs_its_callback_before_the_owner_marker_is_flipped(#[fixture(unicode_home)] home: &Path) {
    let (fx, stub, _) = stub_fixture(home, Attach::Running);
    let vm = vm_with_machine(&fx, Some(stub_machine(&stub)));
    let path = vm.instance().dir().path().to_path_buf();
    let mut owner_at_callback = None;

    let instance = vm
        .hand_over(&mut || {
            owner_at_callback = Some(match read_marker(&path).unwrap() {
                MarkerRead::Valid(marker) => marker.owner,
                _ => panic!("owner.json should be valid"),
            });
        })
        .unwrap();

    assert_eq!(
        owner_at_callback,
        Some(Owner::Process),
        "the VM is released while the process still owns it"
    );
    assert!(matches!(instance.dir().marker().unwrap(), MarkerRead::Valid(m) if m.owner == Owner::None));
}

#[skuld::test]
fn dropping_a_vm_whose_machine_cannot_be_killed_leaves_the_instance_and_says_so(#[fixture(unicode_home)] home: &Path) {
    let (fx, stub, progress) = stub_fixture(home, Attach::Running);
    stub.fail_stops();
    let vm = vm_with_machine(&fx, Some(stub_machine(&stub)));
    let name = vm.name().to_owned();

    drop(vm);

    assert_eq!(stub.calls(), ["attach", "kill"]);
    assert_eq!(
        names(&fx.env.dirs.instances()),
        std::slice::from_ref(&name),
        "a VM that may run keeps its instance"
    );
    let messages = progress.messages();
    assert!(
        messages
            .iter()
            .any(|m| m.contains(&name) && m.contains("could not be killed")),
        "{messages:?}"
    );
}

#[skuld::test]
fn dropping_a_vm_removes_its_ssh_config_entry_and_leaves_the_others(#[fixture(unicode_home)] home: &Path) {
    let (fx, stub, _) = stub_fixture(home, Attach::Running);
    let vm = vm_with_machine(&fx, Some(stub_machine(&stub)));
    let own = write_conf(&fx.engine, home, vm.name());
    let other = write_conf(&fx.engine, home, "other");

    drop(vm);

    assert!(names(&fx.env.dirs.instances()).is_empty());
    assert!(!is_there(&own));
    assert!(is_there(&other), "another instance keeps its entry");
}

#[skuld::test]
fn a_vm_whose_reboot_failed_connects_again_for_its_next_operation_and_keeps_that_session(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = engine_fixture(home, memory());
    fx.guests.stale_boot_reads(usize::MAX);
    let instance = short_bound_instance(&fx, home);
    let vm = vm_over(reached(&instance), instance);
    vm.run(&Command::new(["echo", "before"])).unwrap();
    vm.reboot().expect_err("the guest never leaves its old boot");
    let before = fx.server.connections();

    let output = vm.run(&Command::new(["echo", "after"])).unwrap();
    vm.run(&Command::new(["echo", "again"])).unwrap();

    assert_eq!(
        output.stdout,
        b"after
"
    );
    assert_eq!(
        fx.server.connections() - before,
        1,
        "the failed reboot dropped the session; one connection serves the operations after it"
    );
}

/// A new connection to `instance`, as a started `Vm` holds.
fn reached(instance: &Instance) -> SshSession {
    let dir = instance.dir();
    ops::reach(&instance.engine, dir, &dir.op_shared().unwrap()).unwrap().1
}

/// A `Vm` over `instance` with `session` as its connection.
fn vm_over(session: SshSession, instance: Instance) -> Vm {
    Vm {
        instance: Some(instance),
        machine: Mutex::new(None),
        session: Mutex::new(Some(session)),
    }
}

#[skuld::test]
fn a_vm_whose_session_died_connects_again_for_its_next_operation(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let instance = short_bound_instance(&fx, home);
    let session = reached(&instance);
    let vm = vm_over(session.clone(), instance);
    vm.run(&Command::new(["echo", "before"])).unwrap();
    fx.server.drop_connections();
    while session.is_alive() {
        std::thread::yield_now();
    }
    let before = fx.server.connections();

    let output = vm.run(&Command::new(["echo", "after"])).unwrap();

    assert_eq!(
        output.stdout,
        b"after
"
    );
    assert_eq!(fx.server.connections() - before, 1);
}

/// A new connection to `instance` whose VM watcher ends with the verdict `unwatched` once `end` is sent to.
fn session_with_a_watcher_to_end(
    instance: &Instance,
) -> (SshSession, mpsc::Sender<std::result::Result<(), String>>, VmExit) {
    let (end, ended) = mpsc::channel::<std::result::Result<(), String>>();
    let exit = VmExit::when(move || ended.recv().unwrap_or(Ok(()))).unwrap();
    let dir = instance.dir();
    let running = read_target(&instance.engine, dir, &dir.op_shared().unwrap()).unwrap();
    let session = SshSession::connect(
        &running.target,
        Keepalive::STANDARD,
        exit.clone(),
        None,
        instance.engine.progress().as_ref(),
    )
    .unwrap();
    (session, end, exit)
}

#[skuld::test]
fn a_vm_whose_sessions_watcher_went_unwatched_connects_again_for_its_next_operation(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = engine_fixture(home, memory());
    let instance = short_bound_instance(&fx, home);
    let (session, end, exit) = session_with_a_watcher_to_end(&instance);
    let vm = vm_over(session, instance);
    vm.run(&Command::new(["echo", "before"])).unwrap();
    end.send(Err("the VM process cannot be waited on".to_owned())).unwrap();
    runtime::wait(exit.gone());
    let before = fx.server.connections();

    let output = vm.run(&Command::new(["echo", "after"])).unwrap();

    assert_eq!(
        output.stdout,
        b"after
"
    );
    assert_eq!(fx.server.connections() - before, 1);
}

#[skuld::test]
fn after_a_reboot_a_vm_serves_its_operations_on_the_session_that_found_the_new_boot_ready(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = engine_fixture(home, memory());
    let instance = running_instance(&fx, home);
    let (old, end, exit) = session_with_a_watcher_to_end(&instance);
    let vm = vm_over(old, instance);
    vm.run(&Command::new(["echo", "before"])).unwrap();
    vm.reboot().unwrap();
    // The session to the old boot is unusable from here on; if the Vm still held it, the next operation would connect.
    end.send(Err("the old boot is gone".to_owned())).unwrap();
    runtime::wait(exit.gone());
    let before = fx.server.connections();

    vm.run(&Command::new(["echo", "after"])).unwrap();
    vm.run(&Command::new(["echo", "again"])).unwrap();

    assert_eq!(
        fx.server.connections() - before,
        0,
        "the session the reboot returned serves the operations after it"
    );
}
