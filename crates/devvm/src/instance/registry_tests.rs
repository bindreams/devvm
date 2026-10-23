use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crate::backend::Storage;
use crate::instance::name::InstanceName;
use crate::instance::record::{InstanceRecord, MarkerRead, Owner, RecordRead, read_marker};
use crate::instance::registry::{NewInstance, Owned, Registry};
use crate::store::layers::layers_tests::{Env, build, env, key, names};
use crate::store::layers::{ChainLease, LayerDir};
use crate::test_support::fixtures::{ChannelProgress, MemoryProgress, anna_home};
use crate::{Arch, Dirs, ErrorKind, GuestKind, Network, Result};

/// Writes `disk.img` holding the path of the layer it sits on; optionally fails.
pub(crate) struct TestStorage {
    pub fail: bool,
}

impl Storage for TestStorage {
    fn create_instance_disk(&self, layer: &LayerDir, into: &Path, _size: u64) -> Result<()> {
        if self.fail {
            return Err(crate::Error::new(ErrorKind::Io, "disk creation failed"));
        }
        std::fs::write(into, format!("backing={}", layer.path().display())).unwrap();
        Ok(())
    }

    fn commit_layer(&self, _instance_disk: &Path, _staging: &LayerDir) -> Result<()> {
        unreachable!("the registry never commits layers")
    }
}

pub(crate) struct Fixture {
    pub env: Env,
    pub chain: ChainLease,
    pub registry: Registry,
    pub progress: Arc<MemoryProgress>,
}

pub(crate) fn fixture(home: &Path) -> Fixture {
    let env = env(home);
    let chain = build(&env, key(1), None);
    let progress = Arc::new(MemoryProgress::default());
    let registry = Registry::new(&env.dirs, progress.clone());
    Fixture {
        env,
        chain,
        registry,
        progress,
    }
}

pub(crate) fn new_instance<'a>(fx: &'a Fixture, name: Option<&str>, storage: &'a TestStorage) -> NewInstance<'a> {
    new_instance_on(&fx.chain, name, storage)
}

pub(crate) fn new_instance_on<'a>(
    chain: &'a ChainLease,
    name: Option<&str>,
    storage: &'a TestStorage,
) -> NewInstance<'a> {
    NewInstance {
        name: name.map(|text| InstanceName::parse(text).unwrap()),
        chain,
        guest: GuestKind::Linux,
        arch: Arch::Amd64,
        disk_size: 1 << 30,
        cpus: 2,
        memory: 4 << 30,
        network: Network::Nat,
        created_from: PathBuf::from("/work/project"),
        created: crate::store::layers::layers_tests::t0(),
        storage,
    }
}

pub(crate) fn create(fx: &Fixture, name: Option<&str>) -> Owned {
    let guard = fx.env.store.begin_record().unwrap();
    fx.registry
        .create(new_instance(fx, name, &TestStorage { fail: false }), &guard)
        .unwrap()
}

fn instances(fx: &Fixture) -> PathBuf {
    fx.env.dirs.instances()
}

/// The per-instance lock files; `instances.dir` and `dead.lock` are shared by all instances and never removed.
pub(crate) fn lock_files(fx: &Fixture) -> Vec<String> {
    lock_files_in(&fx.env.dirs)
}

pub(crate) fn lock_files_in(dirs: &Dirs) -> Vec<String> {
    let mut found = names(&dirs.instance_locks());
    found.retain(|name| name.starts_with("instance-"));
    found
}

#[skuld::test]
fn create_makes_a_listed_instance_with_chain_disk_and_creation_dir(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let owned = create(&fx, Some("Foo"));
    let record = owned.dir().record().unwrap();
    assert_eq!(record.name.as_str(), "foo");
    assert_eq!(record.created_from, PathBuf::from("/work/project"));
    assert_eq!(record.chain, fx.chain.refs());
    assert_eq!(record.expires, fx.chain.tip().meta().expires);
    assert!(record.machine.is_none());

    let dir = instances(&fx).join("foo");
    let disk = std::fs::read_to_string(dir.join("disk.img")).unwrap();
    assert_eq!(disk, format!("backing={}", fx.chain.tip().dir().path().display()));
    match read_marker(&dir).unwrap() {
        MarkerRead::Valid(marker) => {
            assert_eq!(marker.owner, Owner::Process);
            assert_eq!(marker.id, *owned.dir().id());
        }
        _ => panic!("owner.json should be valid"),
    }
    assert_eq!(names(&instances(&fx)), ["foo"]);

    let listed = fx.registry.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id(), owned.dir().id());
    let opened = fx.registry.open(&InstanceName::parse("FOO").unwrap()).unwrap();
    assert_eq!(opened.id(), owned.dir().id());
}

#[skuld::test]
fn create_on_a_taken_name_fails_and_leaves_it_untouched(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let first = create(&fx, Some("foo"));
    let before = std::fs::read(instances(&fx).join("foo").join("record.json")).unwrap();
    let locks_before = lock_files(&fx);

    for name in ["foo", "FOO", "Foo"] {
        let guard = fx.env.store.begin_record().unwrap();
        let error = fx
            .registry
            .create(new_instance(&fx, Some(name), &TestStorage { fail: false }), &guard)
            .err()
            .expect("the name is taken");
        assert_eq!(error.kind(), ErrorKind::AlreadyExists, "{name}");
        assert!(error.message().contains("foo"), "{}", error.message());
    }
    assert_eq!(
        std::fs::read(instances(&fx).join("foo").join("record.json")).unwrap(),
        before
    );
    assert_eq!(names(&instances(&fx)), ["foo"], "no .new-* directory is left");
    assert_eq!(lock_files(&fx), locks_before, "no extra lock files");
    assert_eq!(first.dir().record().unwrap().name.as_str(), "foo");
}

#[skuld::test]
fn concurrent_creates_of_one_name_have_exactly_one_winner(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let (parked, at_rename) = mpsc::channel::<()>();
    let (go, proceed) = mpsc::channel::<()>();
    let (loser, winner) = std::thread::scope(|scope| {
        let fx = &fx;
        let a = scope.spawn(move || {
            let guard = fx.env.store.begin_record().unwrap();
            fx.registry.create_with(
                new_instance(fx, Some("foo"), &TestStorage { fail: false }),
                &guard,
                &mut || {
                    parked.send(()).unwrap();
                    proceed.recv().unwrap();
                },
            )
        });
        // A has built its `.new-` directory and is about to rename it. B takes the name first.
        at_rename.recv().unwrap();
        let b = create(fx, Some("foo"));
        go.send(()).unwrap();
        (a.join().unwrap(), b)
    });
    assert_eq!(loser.err().expect("A lost the race").kind(), ErrorKind::AlreadyExists);
    assert_eq!(names(&instances(&fx)), ["foo"], "A's .new-* directory is gone");
    assert_eq!(lock_files(&fx).len(), 2, "only B's two lock files remain");
    assert_eq!(
        fx.registry.open(&InstanceName::parse("foo").unwrap()).unwrap().id(),
        winner.dir().id()
    );
}

#[skuld::test]
fn create_with_a_failing_storage_leaves_nothing(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let guard = fx.env.store.begin_record().unwrap();
    let error = fx
        .registry
        .create(new_instance(&fx, Some("foo"), &TestStorage { fail: true }), &guard)
        .err()
        .expect("the storage fails");
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(names(&instances(&fx)).is_empty());
    assert!(lock_files(&fx).is_empty());
}

#[skuld::test]
fn two_creates_without_names_get_different_valid_names(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let first = create(&fx, None);
    let second = create(&fx, None);
    let (a, b) = (first.dir().name().as_str(), second.dir().name().as_str());
    assert_ne!(a, b);
    for name in [a, b] {
        assert!(InstanceName::parse(name).is_ok());
        assert!(instances(&fx).join(name).is_dir());
    }
}

#[skuld::test]
fn make_ownerless_disarms_first_then_flips_while_the_lock_is_held(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let owned = create(&fx, Some("foo"));
    let dir = instances(&fx).join("foo");
    let id = *owned.dir().id();
    let locks = fx.registry.locks();
    let seen = Mutex::new(Vec::new());
    let ownerless = owned
        .make_ownerless(&mut || {
            let marker = match read_marker(&dir).unwrap() {
                MarkerRead::Valid(marker) => marker.owner,
                _ => panic!("owner.json should be valid"),
            };
            let held = matches!(locks.try_owner(&id).unwrap(), crate::instance::locks::Probe::Held);
            seen.lock().unwrap().push((marker, held));
            Ok(())
        })
        .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        [(Owner::Process, true)],
        "disarm runs while the marker still says process and the owner lock is held"
    );
    match read_marker(&dir).unwrap() {
        MarkerRead::Valid(marker) => assert_eq!(marker.owner, Owner::None),
        _ => panic!("owner.json should be valid"),
    }
    assert!(
        matches!(locks.try_owner(&id).unwrap(), crate::instance::locks::Probe::Free(_)),
        "the lock is released after the flip"
    );
    assert_eq!(ownerless.id(), &id);
}

#[skuld::test]
fn make_ownerless_with_a_failing_disarm_flips_nothing(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let owned = create(&fx, Some("foo"));
    let dir = instances(&fx).join("foo");
    let id = *owned.dir().id();
    let error = owned
        .make_ownerless(&mut || Err(crate::Error::new(ErrorKind::Io, "cannot disarm")))
        .err()
        .expect("the disarm fails");
    assert_eq!(error.message(), "cannot disarm");
    match read_marker(&dir).unwrap() {
        MarkerRead::Valid(marker) => assert_eq!(marker.owner, Owner::Process),
        _ => panic!("owner.json should be valid"),
    }
    assert!(
        matches!(
            fx.registry.locks().try_owner(&id).unwrap(),
            crate::instance::locks::Probe::Free(_)
        ),
        "the lock is released, so the reaper may take the instance"
    );
}

#[skuld::test]
fn a_bad_name_is_rejected_by_parse_and_the_state_directory_is_never_created(#[fixture(anna_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let registry = Registry::new(&dirs, Arc::new(MemoryProgress::default()));
    for text in ["../x", "con", "a*", &"a".repeat(64)] {
        assert_eq!(InstanceName::parse(text).unwrap_err().kind(), ErrorKind::InvalidName);
    }
    let missing = InstanceName::parse("nothing").unwrap();
    assert_eq!(registry.open(&missing).err().unwrap().kind(), ErrorKind::NotFound);
    assert!(registry.list().unwrap().is_empty());
    assert!(!dirs.state.exists(), "reading must not create the state directory");
}

#[skuld::test]
fn list_skips_dot_dirs_and_warns_about_an_unreadable_record(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let good = create(&fx, Some("good"));
    let broken = create(&fx, Some("broken"));
    std::fs::write(instances(&fx).join("broken").join("record.json"), "{ not json").unwrap();
    std::fs::create_dir(instances(&fx).join(".new-0000")).unwrap();
    std::fs::create_dir(instances(&fx).join(".dead-0000")).unwrap();
    std::fs::create_dir(instances(&fx).join("empty")).unwrap();

    let listed = fx.registry.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id(), good.dir().id());
    let warnings = fx.progress.messages();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("broken"), "{}", warnings[0]);
    drop(broken);
}

#[skuld::test]
fn open_of_a_missing_instance_is_not_found(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let error = fx
        .registry
        .open(&InstanceName::parse("nothing").unwrap())
        .err()
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(error.message().contains("nothing"), "{}", error.message());
}

#[skuld::test]
fn update_rewrites_the_record_under_the_op_lock(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let owned = create(&fx, Some("foo"));
    let dir = owned.dir();
    let op = dir.op_exclusive().unwrap();
    let updated = dir
        .update(&op, &mut |record: &mut InstanceRecord| record.cpus = 7)
        .unwrap();
    assert_eq!(updated.cpus, 7);
    assert_eq!(dir.record().unwrap().cpus, 7);
}

#[skuld::test]
fn destroy_removes_the_dir_and_both_lock_files(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let owned = create(&fx, Some("foo"));
    let ownerless = owned.make_ownerless(&mut || Ok(())).unwrap();
    let op = ownerless.op_exclusive().unwrap();
    ownerless.destroy(op).unwrap();
    assert!(names(&instances(&fx)).is_empty(), "{:?}", names(&instances(&fx)));
    assert!(lock_files(&fx).is_empty());
    assert_eq!(ownerless.record().unwrap_err().kind(), ErrorKind::NotFound);
}

#[skuld::test]
fn a_waiter_on_the_op_lock_of_a_removed_and_recreated_name_gets_not_found(#[fixture(anna_home)] home: &Path) {
    let (progress, messages) = ChannelProgress::new();
    let env = env(home);
    let chain = build(&env, key(1), None);
    let registry = Registry::new(&env.dirs, progress);
    let fx = Fixture {
        env,
        chain,
        registry,
        progress: Arc::new(MemoryProgress::default()),
    };
    let owned = create(&fx, Some("foo"));
    let first = owned.make_ownerless(&mut || Ok(())).unwrap();
    let waiter_handle = fx.registry.open(&InstanceName::parse("foo").unwrap()).unwrap();
    let held = first.op_exclusive().unwrap();

    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| waiter_handle.op_exclusive().map(drop));
        assert!(messages.recv().unwrap().starts_with("waiting for"));

        // Remove the instance behind the holder's back, then create a new one under the same name. The holder still
        // has the old op lock; the waiter wakes on it only once the holder lets go.
        std::fs::remove_dir_all(instances(&fx).join("foo")).unwrap();
        fx.registry.locks().remove(first.id()).unwrap();
        let second = create(&fx, Some("foo"));
        let second_record = std::fs::read(instances(&fx).join("foo").join("record.json")).unwrap();
        drop(held);

        let error = waiter.join().unwrap().expect_err("the waited-for instance is gone");
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(
            std::fs::read(instances(&fx).join("foo").join("record.json")).unwrap(),
            second_record,
            "the new instance is intact"
        );
        assert_ne!(second.dir().id(), first.id());
    });
}

#[skuld::test]
fn a_handle_whose_name_now_belongs_to_another_instance_gets_not_found(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let first = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    // The first instance's directory is replaced, but its lock files stay: only the id check can tell.
    std::fs::remove_dir_all(instances(&fx).join("foo")).unwrap();
    let second = create(&fx, Some("foo"));

    assert_eq!(first.record().unwrap_err().kind(), ErrorKind::NotFound);
    assert_eq!(first.op_exclusive().err().unwrap().kind(), ErrorKind::NotFound);
    assert_eq!(first.op_shared().err().unwrap().kind(), ErrorKind::NotFound);
    assert_eq!(first.try_op_exclusive().err().unwrap().kind(), ErrorKind::NotFound);
    assert!(matches!(
        InstanceRecord::read(&instances(&fx).join("foo")).unwrap(),
        RecordRead::Valid(record) if record.id == *second.dir().id()
    ));
}

#[skuld::test]
fn update_and_destroy_refuse_a_guard_of_another_instance(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let a = create(&fx, Some("a")).make_ownerless(&mut || Ok(())).unwrap();
    let b = create(&fx, Some("b")).make_ownerless(&mut || Ok(())).unwrap();
    let op_b = b.op_exclusive().unwrap();
    let error = a.update(&op_b, &mut |_| {}).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Internal);
    assert_eq!(a.destroy(op_b).unwrap_err().kind(), ErrorKind::Internal);
    assert_eq!(names(&instances(&fx)), ["a", "b"], "nothing was deleted");
}

#[skuld::test]
fn a_shared_guard_lets_readers_coexist_but_excludes_the_exclusive_op(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let first = dir.op_shared().unwrap();
    let second = dir.op_shared().unwrap();
    assert!(dir.try_op_exclusive().unwrap().is_none());
    drop((first, second));
    assert!(dir.try_op_exclusive().unwrap().is_some());
}

#[skuld::test]
fn placing_a_new_instance_waits_for_readers_of_instance_files(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let existing = create(&fx, Some("bar")).make_ownerless(&mut || Ok(())).unwrap();
    let (contended, contention) = mpsc::channel::<()>();

    let placed_while_reading = std::thread::scope(|scope| {
        let fx = &fx;
        let creator = fx.registry.locks().reading(existing.path(), || {
            let creator = scope.spawn(move || {
                let guard = fx.env.store.begin_record().unwrap();
                fx.registry.create_with_waits(
                    new_instance(fx, Some("foo"), &TestStorage { fail: false }),
                    &guard,
                    &mut || {},
                    &mut || contended.send(()).unwrap(),
                )
            });
            // The creator is at its placement and found the readers' lock taken: `foo` is not there yet.
            // `contended` is dropped with the creator if it never waits, so this fails instead of hanging.
            let waited = contention.recv().is_ok();
            (creator, waited, instances(fx).join("foo").exists())
        });
        let (creator, waited, placed_early) = creator.unwrap().unwrap();
        creator.join().unwrap().unwrap();
        (waited, placed_early)
    });
    assert_eq!(
        placed_while_reading,
        (true, false),
        "the creator placed `foo` while a reader had files open"
    );
    assert_eq!(names(&instances(&fx)), ["bar", "foo"]);
}
