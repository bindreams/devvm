use std::path::Path;

use crate::ErrorKind;
use crate::instance::registry::registry_tests::{Fixture, TestStorage, create, fixture, new_instance};
use crate::store::layers::layers_tests::{build, build_expiring, days, key, t0};
use crate::store::layers::{LayerRef, Reachability};
use crate::test_support::fixtures::anna_home;

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
fn chains_lists_every_instance_chain_running_or_stopped(#[fixture(anna_home)] home: &Path) {
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
fn chains_does_not_wait_on_held_locks(#[fixture(anna_home)] home: &Path) {
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
fn chains_fails_naming_a_corrupt_record(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    drop(create(&fx, Some("foo")));
    std::fs::write(fx.env.dirs.instances().join("foo").join("record.json"), "{ torn").unwrap();
    let error = fx.registry.chains().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Store);
    assert!(error.message().contains("foo"), "{}", error.message());
}

#[skuld::test]
fn chains_fails_on_a_record_from_a_newer_devvm(#[fixture(anna_home)] home: &Path) {
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
fn chains_ignores_dot_dirs_and_directories_without_a_record(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let root = fx.env.dirs.instances();
    std::fs::create_dir_all(root.join(".new-00")).unwrap();
    std::fs::create_dir_all(root.join(".dead-00")).unwrap();
    std::fs::create_dir_all(root.join("vanishing")).unwrap();
    assert!(fx.registry.chains().unwrap().is_empty());
}

#[skuld::test]
fn gc_keeps_the_generations_of_an_instance_on_a_superseded_generation(#[fixture(anna_home)] home: &Path) {
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
