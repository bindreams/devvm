use std::path::Path;
use std::sync::{Arc, Mutex};

use super::availability_on;
use crate::backend::{Backend, Machine};
use crate::engine::engine_tests::{EngineFixture, dead_machine, engine_fixture, image_of, stub_fixture};
use crate::instance::record::InstanceId;
use crate::instance::record::{MarkerRead, Owner, read_marker};
use crate::store::layers::layers_tests::names;
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::host::{NO_ACCELERATOR, NoAccelHost, TestHost};
use crate::test_support::stub_backend::{Attach, StubBackend};
use crate::vm::Handle;
use crate::{Command, ErrorKind, Guest, Image, Instance, InstanceState, ListFilter, StartOptions, Vm, availability};

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
fn image_create_reaps_a_dead_creators_leftovers_first(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let image = image_of(&fx.engine);
    // A creator that died: its `.new-` directory is left, and nobody holds its owner lock.
    let id = InstanceId::random();
    drop(fx.engine.registry().locks().acquire_owner_new(&id).unwrap());
    let leftover = fx.env.dirs.instances().join(format!(".new-{id}"));
    std::fs::create_dir_all(&leftover).unwrap();

    image.create("foo", &StartOptions::default()).unwrap();

    assert!(!leftover.exists(), "creating an instance reaps leftovers first");
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
fn guest_methods_are_not_supported_until_the_access_layer(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let instance = image_of(&fx.engine).create("foo", &StartOptions::default()).unwrap();

    let results = [
        instance.run(&Command::new(["true"])).map(drop),
        instance.reboot(),
        instance.copy_in(Path::new("a"), "/b"),
        instance.copy_out("/b", Path::new("a")),
    ];
    for result in results {
        let error = result.expect_err("the access layer is not built yet");
        assert_eq!(error.kind(), ErrorKind::NotSupported);
    }
}

#[skuld::test]
fn availability_names_the_missing_piece() {
    let missing = availability_on(&NoAccelHost).expect_err("this host has no hypervisor");
    assert_eq!(missing.to_string(), NO_ACCELERATOR);
}

#[skuld::test]
fn availability_is_ok_where_an_accelerator_exists() {
    availability_on(&TestHost).expect("the test host accelerates");
}

#[skuld::test]
fn the_public_availability_asks_the_host_this_binary_runs_on() {
    assert_eq!(
        availability().map_err(|e| e.to_string()),
        availability_on(crate::host::current()).map_err(|e| e.to_string())
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
