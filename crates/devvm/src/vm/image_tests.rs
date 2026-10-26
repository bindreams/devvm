use std::path::Path;
use std::sync::Arc;

use crate::engine::engine_tests::{engine_fixture, image_of};
use crate::instance::record::InstanceId;
use crate::test_support::fixtures::{MemoryProgress, anna_home};
use crate::{Command, ErrorKind, Guest, Image, Instance, InstanceState, ListFilter, StartOptions, Vm, availability};

fn memory() -> Arc<MemoryProgress> {
    Arc::new(MemoryProgress::default())
}

#[skuld::test]
fn image_create_leaves_a_stopped_ownerless_instance(#[fixture(anna_home)] home: &Path) {
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
fn image_create_reaps_a_dead_creators_leftovers_first(#[fixture(anna_home)] home: &Path) {
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
fn a_bad_instance_name_is_rejected_before_anything_is_created(#[fixture(anna_home)] home: &Path) {
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
fn guest_methods_are_not_supported_until_the_access_layer(#[fixture(anna_home)] home: &Path) {
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
    let missing = availability().expect_err("no accelerator is detected yet");
    assert!(!missing.to_string().is_empty());
}
