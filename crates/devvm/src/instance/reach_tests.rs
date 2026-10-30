use std::path::Path;

use crate::ErrorKind;
use crate::instance::reach::entry_is_dir;
use crate::instance::registry::registry_tests::{Fixture, TestStorage, create, fixture, new_instance};
use crate::store::layers::layers_tests::{build, build_expiring, days, key, t0};
use crate::store::layers::{LayerRef, Reachability};
use crate::test_support::fixtures::unicode_home;

fn sorted(mut chains: Vec<Vec<LayerRef>>) -> Vec<Vec<LayerRef>> {
    chains.sort_by_key(|chain| {
        chain
            .iter()
            .map(|layer| layer.generation.to_string())
            .collect::<Vec<_>>()
    });
    chains
}

#[skuld::test]
fn chains_lists_every_instance_chain_running_or_stopped(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let base = fx.chain.refs();
    let child = build(&fx.env, key(2), Some(&fx.chain));

    let owned = create(&fx, Some("owned"));
    let ownerless = create(&fx, Some("ownerless")).make_ownerless(&mut || Ok(())).unwrap();
    let guard = fx.env.store.begin_record().unwrap();
    let on_child = fx
        .registry
        .create(
            crate::instance::registry::NewInstance {
                chain: &child,
                ..new_instance(&fx, Some("on-child"), &TestStorage { fail: false })
            },
            &guard,
        )
        .unwrap();
    drop(guard);

    let chains = sorted(fx.registry.chains().unwrap());
    assert_eq!(chains.len(), 3);
    assert_eq!(chains.iter().filter(|chain| **chain == base).count(), 2);
    assert!(chains.contains(&child.refs()));
    assert_eq!(child.refs().len(), 2, "a chain lists every ancestor, root first");
    drop((owned, ownerless, on_child));
}

#[skuld::test]
fn chains_keeps_an_instance_whose_dir_lock_file_was_deleted_from_outside(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let owned = create(&fx, Some("foo"));
    std::fs::remove_file(fx.env.dirs.instance_locks().join("instances.dir")).unwrap();

    assert_eq!(
        fx.registry.chains().unwrap(),
        [fx.chain.refs()],
        "a live chain must not read as absent, or gc deletes its layers"
    );
    assert!(
        fx.registry
            .open(&crate::instance::name::InstanceName::parse("foo").unwrap())
            .is_ok()
    );
    drop(owned);
}

#[skuld::test]
fn an_entry_whose_type_cannot_be_read_fails_the_call_instead_of_being_skipped() {
    let path = Path::new("instances/foo");
    let error = entry_is_dir(path, Err(std::io::Error::other("the type is unreadable"))).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(
        error.message().contains("foo") && error.message().contains("nothing was deleted"),
        "{}",
        error.message()
    );
}

#[skuld::test]
fn an_entry_that_is_a_directory_is_one_and_a_file_is_not(#[fixture(unicode_home)] home: &Path) {
    let file = home.join("file");
    std::fs::write(&file, b"x").unwrap();
    for (path, expected) in [(home.to_path_buf(), true), (file, false)] {
        assert_eq!(
            entry_is_dir(&path, std::fs::metadata(&path).map(|m| m.file_type())).unwrap(),
            expected
        );
    }
}

#[skuld::test]
fn chains_does_not_wait_on_held_locks(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let owned = create(&fx, Some("foo"));
    let dir = fx
        .registry
        .open(&crate::instance::name::InstanceName::parse("foo").unwrap())
        .unwrap();
    let op = dir.op_exclusive().unwrap();
    // The owner lock is held by `owned` and the op lock by `op`; a call that waited on either would hang here.
    assert_eq!(fx.registry.chains().unwrap().len(), 1);
    drop((owned, op));
}

#[skuld::test]
fn chains_fails_naming_a_corrupt_record(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    drop(create(&fx, Some("foo")));
    std::fs::write(fx.env.dirs.instances().join("foo").join("record.json"), "{ torn").unwrap();
    let error = fx.registry.chains().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Store);
    assert!(error.message().contains("foo"), "{}", error.message());
}

#[skuld::test]
fn chains_keeps_the_chain_of_a_quarantined_instance(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    drop(create(&fx, Some("foo")));
    std::fs::remove_file(fx.env.dirs.instances().join("foo").join("owner.json")).unwrap();
    let report = fx.registry.reap();
    assert_eq!(report.quarantined.len(), 1, "{report:?}");
    assert!(!fx.env.dirs.instances().join("foo").exists());

    assert_eq!(
        fx.registry.chains().unwrap(),
        [fx.chain.refs()],
        "quarantine never deletes the disk, so gc must not delete its layers"
    );
}

#[skuld::test]
fn chains_fails_naming_a_quarantined_instance_whose_record_is_unreadable(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    drop(create(&fx, Some("foo")));
    std::fs::write(fx.env.dirs.instances().join("foo").join("record.json"), "{ torn").unwrap();
    let report = fx.registry.reap();
    assert_eq!(report.quarantined.len(), 1, "{report:?}");

    let error = fx.registry.chains().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Store);
    assert!(
        error.message().contains("quarantine") && error.message().contains("nothing was deleted"),
        "{}",
        error.message()
    );
}

#[skuld::test]
fn chains_fails_naming_a_quarantined_instance_whose_record_is_missing(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    drop(create(&fx, Some("foo")));
    std::fs::remove_file(fx.env.dirs.instances().join("foo").join("record.json")).unwrap();
    let report = fx.registry.reap();
    assert_eq!(report.quarantined.len(), 1, "{report:?}");
    let quarantined = report.quarantined[0].clone();

    let error = fx.registry.chains().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Store);
    assert!(
        error.message().contains("quarantined instance")
            && error.message().contains("record.json is missing")
            && error.message().contains(&quarantined.display().to_string())
            && error.message().contains("nothing was deleted"),
        "{}",
        error.message()
    );
}

#[skuld::test]
fn chains_fails_on_a_record_from_a_newer_devvm(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    drop(create(&fx, Some("foo")));
    std::fs::write(
        fx.env.dirs.instances().join("foo").join("record.json"),
        r#"{"schema": 2}"#,
    )
    .unwrap();
    let error = fx.registry.chains().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Store);
    assert!(error.message().contains("newer devvm"), "{}", error.message());
}

#[skuld::test]
fn chains_ignores_dot_dirs_and_directories_without_a_record(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let root = fx.env.dirs.instances();
    std::fs::create_dir_all(root.join(".new-00")).unwrap();
    std::fs::create_dir_all(root.join(".dead-00")).unwrap();
    std::fs::create_dir_all(root.join("vanishing")).unwrap();
    assert!(fx.registry.chains().unwrap().is_empty());
}

#[skuld::test]
fn gc_keeps_the_generations_of_an_instance_on_a_superseded_generation(#[fixture(unicode_home)] home: &Path) {
    let Fixture {
        env,
        chain: unrelated,
        registry,
        progress: _,
    } = fixture(home);
    drop(unrelated);
    let storage = TestStorage { fail: false };
    let chain = build_expiring(&env, key(5), None, Some(t0() + days(90)));
    let first = chain.tip().layer_ref();
    let guard = env.store.begin_record().unwrap();
    let instance = registry
        .create(
            crate::instance::registry::NewInstance {
                name: Some(crate::instance::name::InstanceName::parse("foo").unwrap()),
                chain: &chain,
                guest: crate::GuestKind::Linux,
                arch: crate::Arch::Amd64,
                disk_size: 1 << 20,
                cpus: 1,
                memory: 1 << 30,
                network: crate::Network::Nat,
                created_from: home.to_path_buf(),
                created: t0(),
                storage: &storage,
            },
            &guard,
        )
        .unwrap()
        .make_ownerless(&mut || Ok(()))
        .unwrap();
    drop(guard);
    drop(chain);

    // The key is rebuilt after its first generation entered the expiry margin: the first generation is superseded
    // and nothing but the instance refers to it.
    env.clock.set(t0() + days(85));
    let second = build_expiring(&env, key(5), None, Some(t0() + days(85) + days(90)));
    assert_ne!(second.tip().layer_ref(), first);
    drop(second);

    let generation = env.store.generation_dir(&first);
    let report = env.store.gc(&registry).unwrap();
    assert!(report.deleted.is_empty(), "{report:?}");
    assert!(
        generation.join("disk.qcow2").is_file(),
        "the instance keeps its generation alive"
    );

    let op = instance.op_exclusive().unwrap();
    instance.destroy(op).unwrap();
    let report = env.store.gc(&registry).unwrap();
    assert_eq!(report.deleted, [first]);
    assert!(!generation.exists());
}
