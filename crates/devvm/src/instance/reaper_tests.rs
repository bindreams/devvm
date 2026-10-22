use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::backend::MachineRecord;
use crate::contain::VmProcess;
use crate::contain::contain_tests::Sleeper;
use crate::instance::dir::InstanceDir;
use crate::instance::locks::Probe;
use crate::instance::name::InstanceName;
use crate::instance::reaper::ReapReport;
use crate::instance::record::{InstanceId, InstanceRecord, MarkerRead, Owner, RecordRead, read_marker};
use crate::instance::registry::registry_tests::{Fixture, create, fixture, lock_files};
use crate::store::layers::layers_tests::names;
use crate::test_support::fixtures::anna_home;

fn machine(sleeper: &Sleeper) -> MachineRecord {
    MachineRecord {
        process: sleeper.record(),
        qmp: String::new(),
        ssh: Vec::new(),
    }
}

fn set_machine(dir: &InstanceDir, machine: MachineRecord) {
    let op = dir.op_exclusive().unwrap();
    dir.update(&op, &mut |record: &mut InstanceRecord| {
        record.machine = Some(machine.clone())
    })
    .unwrap();
}

fn root(fx: &Fixture) -> PathBuf {
    fx.env.dirs.instances()
}

fn quarantined(fx: &Fixture) -> Vec<String> {
    names(&fx.env.dirs.instance_quarantine().join("instances"))
}

fn alive(record: &MachineRecord) -> bool {
    VmProcess::attach(&record.process).unwrap().is_some()
}

#[skuld::test]
fn an_owned_instance_with_a_dead_owner_is_reaped_with_its_vm(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let sleeper = Sleeper::spawn();
    let owned = create(&fx, Some("foo"));
    set_machine(owned.dir(), machine(&sleeper));
    drop(owned);

    let report = fx.registry.reap();
    assert_eq!(report.reaped, ["foo"]);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert!(
        !alive(&machine(&sleeper)),
        "the recorded VM is killed, and kill waited for its exit"
    );
    assert!(names(&root(&fx)).is_empty());
    assert!(lock_files(&fx).is_empty());
}

#[skuld::test]
fn a_live_owner_is_never_reaped(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let sleeper = Sleeper::spawn();
    let owned = create(&fx, Some("foo"));
    set_machine(owned.dir(), machine(&sleeper));
    let before = std::fs::read(root(&fx).join("foo").join("record.json")).unwrap();

    let report = fx.registry.reap();
    assert!(report.reaped.is_empty() && report.warnings.is_empty() && report.quarantined.is_empty());
    assert_eq!(
        std::fs::read(root(&fx).join("foo").join("record.json")).unwrap(),
        before
    );
    assert!(alive(&machine(&sleeper)));
    drop(owned);
}

#[skuld::test]
fn an_ownerless_instance_is_never_reaped(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let sleeper = Sleeper::spawn();
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    set_machine(&dir, machine(&sleeper));

    let report = fx.registry.reap();
    assert!(report.reaped.is_empty() && report.warnings.is_empty());
    assert!(dir.record().is_ok());
    assert!(alive(&machine(&sleeper)));
    assert_eq!(lock_files(&fx).len(), 2, "its lock files stay");
}

#[skuld::test]
fn a_recorded_pid_with_another_start_token_is_not_killed(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let sleeper = Sleeper::spawn();
    let mut forged = machine(&sleeper);
    forged.process.token ^= 1;
    let owned = create(&fx, Some("foo"));
    set_machine(owned.dir(), forged);
    drop(owned);

    let report = fx.registry.reap();
    assert_eq!(report.reaped, ["foo"], "the instance goes");
    assert!(
        alive(&machine(&sleeper)),
        "a process that is not the recorded one is not touched"
    );
}

#[skuld::test]
fn a_dead_creators_new_dir_and_lock_files_are_removed(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let id = InstanceId::random();
    let locks = fx.registry.locks();
    let dead = locks.acquire_owner_new(&id).unwrap();
    let new_dir = root(&fx).join(format!(".new-{id}"));
    std::fs::create_dir_all(&new_dir).unwrap();
    std::fs::write(new_dir.join("disk.img"), "x").unwrap();

    let report = fx.registry.reap();
    assert!(report.reaped.is_empty(), "a live creator's directory is left alone");
    assert!(new_dir.is_dir());

    drop(dead);
    let report = fx.registry.reap();
    assert!(!new_dir.exists());
    assert!(lock_files(&fx).is_empty());
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[skuld::test]
fn a_new_dir_placed_after_the_listing_keeps_its_lock_files(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let id = *dir.id();
    // The reaper listed `.new-<id>`, then the creator renamed it to `foo` and let go of its owner lock.
    let mut report = ReapReport::default();
    fx.registry.reap_new(
        &id.to_string(),
        &format!(".new-{id}"),
        &root(&fx).join(format!(".new-{id}")),
        &mut report,
    );
    assert!(report.warnings.is_empty(), "{report:?}");
    assert_eq!(lock_files(&fx).len(), 2, "the placed instance keeps both lock files");
    assert!(dir.op_exclusive().is_ok());
}

#[skuld::test]
fn a_new_dir_with_no_lock_file_is_removed(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let new_dir = root(&fx).join(format!(".new-{}", InstanceId::random()));
    std::fs::create_dir_all(&new_dir).unwrap();
    fx.registry.reap();
    assert!(!new_dir.exists());
}

#[skuld::test]
fn an_orphan_lock_file_is_swept_but_an_ownerless_instances_locks_are_not(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let orphan = InstanceId::random();
    drop(fx.registry.locks().acquire_owner_new(&orphan).unwrap());
    let kept = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    assert_eq!(lock_files(&fx).len(), 4);

    fx.registry.reap();
    assert!(!fx.registry.locks().owner_path(&orphan).exists());
    assert!(!fx.registry.locks().op_path(&orphan).exists());
    assert!(fx.registry.locks().owner_path(kept.id()).exists());
    assert!(fx.registry.locks().op_path(kept.id()).exists());
}

#[skuld::test]
fn the_lock_sweep_lists_the_instance_directories_once_however_many_locks_there_are(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let orphans: Vec<InstanceId> = (0..10).map(|_| InstanceId::random()).collect();
    for id in &orphans {
        drop(fx.registry.locks().acquire_owner_new(id).unwrap());
    }
    let kept = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();

    let mut listings = 0;
    fx.registry
        .sweep_locks_with(&mut ReapReport::default(), &mut || listings += 1);

    assert_eq!(listings, 1);
    assert_eq!(lock_files(&fx).len(), 2, "only the instance's lock files are left");
    assert!(fx.registry.locks().owner_path(kept.id()).exists());
}

#[skuld::test]
fn a_dead_dir_is_deleted(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let dead = root(&fx).join(format!(".dead-{}", InstanceId::random()));
    std::fs::create_dir_all(dead.join("sub")).unwrap();
    std::fs::write(dead.join("sub").join("disk.img"), "x").unwrap();
    fx.registry.reap();
    assert!(!dead.exists());
}

#[skuld::test]
fn corrupt_record_missing_marker_and_torn_marker_are_quarantined_not_deleted(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let cases: [(&str, &str, Option<&str>); 3] = [
        ("bad-record", "record.json", Some("{ torn")),
        ("no-marker", "owner.json", None),
        ("bad-marker", "owner.json", Some("{\"schema\":1,\"own")),
    ];
    for (name, file, content) in cases {
        let owned = create(&fx, Some(name));
        drop(owned);
        let path = root(&fx).join(name).join(file);
        match content {
            Some(text) => std::fs::write(&path, text).unwrap(),
            None => std::fs::remove_file(&path).unwrap(),
        }
        let report = fx.registry.reap();
        assert_eq!(report.quarantined.len(), 1, "{name}: {report:?}");
        assert!(!root(&fx).join(name).exists(), "{name}: the instance leaves instances/");
        let moved = report.quarantined[0].clone();
        assert!(
            moved.starts_with(fx.env.dirs.instance_quarantine().join("instances")),
            "{moved:?}"
        );
        assert!(moved.join("disk.img").is_file(), "{name}: its content is preserved");
        if let Some(text) = content {
            assert_eq!(std::fs::read_to_string(moved.join(file)).unwrap(), text, "{name}");
        }
    }
    assert_eq!(quarantined(&fx).len(), 3);
}

#[skuld::test]
fn a_corrupt_record_under_a_held_owner_lock_is_not_moved(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let owned = create(&fx, Some("foo"));
    std::fs::write(root(&fx).join("foo").join("record.json"), "{ torn").unwrap();
    let report = fx.registry.reap();
    assert!(report.quarantined.is_empty(), "{report:?}");
    assert!(root(&fx).join("foo").is_dir());
    drop(owned);
    assert_eq!(
        fx.registry.reap().quarantined.len(),
        1,
        "once its owner is gone it is moved"
    );
}

#[skuld::test]
fn a_record_or_marker_from_a_newer_devvm_is_left_alone_with_a_warning(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let sleeper = Sleeper::spawn();
    for (name, file) in [("newer-record", "record.json"), ("newer-marker", "owner.json")] {
        let owned = create(&fx, Some(name));
        set_machine(owned.dir(), machine(&sleeper));
        drop(owned);
        let path = root(&fx).join(name).join(file);
        std::fs::write(&path, r#"{"schema": 2, "from": "the future"}"#).unwrap();

        let report = fx.registry.reap();
        assert_eq!(report.warnings.len(), 1, "{name}: {report:?}");
        assert!(report.warnings[0].contains(name), "{}", report.warnings[0]);
        assert!(
            report.reaped.is_empty() && report.quarantined.is_empty(),
            "{name}: {report:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"schema": 2, "from": "the future"}"#
        );
        assert!(root(&fx).join(name).join("disk.img").is_file());
        assert!(alive(&machine(&sleeper)), "{name}: nothing is killed");
        // Taken out of the way so the next iteration's warning count is its own.
        std::fs::rename(root(&fx).join(name), home.join(name)).unwrap();
    }
    assert!(quarantined(&fx).is_empty());
}

#[skuld::test]
fn an_op_lock_held_defers_the_reap_without_a_warning(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let owned = create(&fx, Some("foo"));
    let dir = fx.registry.open(&InstanceName::parse("foo").unwrap()).unwrap();
    let op = dir.op_exclusive().unwrap();
    drop(owned);

    let report = fx.registry.reap();
    assert!(report.reaped.is_empty() && report.warnings.is_empty(), "{report:?}");
    assert!(root(&fx).join("foo").is_dir());
    drop(op);
    assert_eq!(fx.registry.reap().reaped, ["foo"]);
}

#[skuld::test]
fn an_owned_instance_whose_lock_files_are_gone_is_still_reaped(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let owned = create(&fx, Some("foo"));
    let id = *owned.dir().id();
    drop(owned);
    fx.registry.locks().remove(&id).unwrap();
    let report = fx.registry.reap();
    assert_eq!(report.reaped, ["foo"], "{report:?}");
    assert!(report.warnings.is_empty(), "{report:?}");
    assert!(names(&root(&fx)).is_empty());
    assert!(lock_files(&fx).is_empty());
}

#[skuld::test]
fn an_ownerless_instance_whose_lock_files_are_gone_gets_them_back(#[fixture(anna_home)] home: &Path) {
    for (case, gone) in [("owner", 1), ("op", 2), ("both", 3)] {
        let fx = fixture(&home.join(case));
        let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
        let locks = fx.registry.locks();
        if gone & 1 != 0 {
            std::fs::remove_file(locks.owner_path(dir.id())).unwrap();
        }
        if gone & 2 != 0 {
            std::fs::remove_file(locks.op_path(dir.id())).unwrap();
        }

        let report = fx.registry.reap();
        assert!(
            report.reaped.is_empty() && report.warnings.is_empty(),
            "{case}: {report:?}"
        );
        assert_eq!(lock_files(&fx).len(), 2, "{case}: both lock files are back");
        let op = dir.op_exclusive().unwrap();
        dir.destroy(op).unwrap();
        assert!(names(&root(&fx)).is_empty(), "{case}");
        assert!(lock_files(&fx).is_empty(), "{case}");
    }
}

#[skuld::test]
fn create_in_a_loop_while_a_reaper_loop_runs_never_loses_a_live_instance(#[fixture(anna_home)] home: &Path) {
    const INSTANCES: usize = 40;
    let fx = fixture(home);
    let done = AtomicBool::new(false);
    let kept = std::thread::scope(|scope| {
        let reaper = scope.spawn(|| {
            // Ends when the creator says it is done: the awaited condition, not a time.
            while !done.load(Ordering::Acquire) {
                let report = fx.registry.reap();
                assert!(report.reaped.is_empty(), "a live instance was reaped: {report:?}");
                assert!(report.quarantined.is_empty(), "{report:?}");
            }
        });
        let mut kept = Vec::new();
        for i in 0..INSTANCES {
            let owned = create(&fx, Some(&format!("i{i}")));
            if i % 2 == 0 {
                kept.push(owned);
            } else {
                owned.make_ownerless(&mut || Ok(())).unwrap();
            }
        }
        done.store(true, Ordering::Release);
        reaper.join().unwrap();
        kept
    });

    assert_eq!(fx.registry.list().unwrap().len(), INSTANCES);
    assert_eq!(
        lock_files(&fx).len(),
        2 * INSTANCES,
        "every instance keeps both lock files"
    );
    for owned in &kept {
        assert!(matches!(
            InstanceRecord::read(owned.dir().path()).unwrap(),
            RecordRead::Valid(_)
        ));
        match read_marker(owned.dir().path()).unwrap() {
            MarkerRead::Valid(marker) => assert_eq!(marker.owner, Owner::Process),
            _ => panic!("the marker of a live instance is intact"),
        }
        assert!(matches!(
            fx.registry.locks().try_owner(owned.dir().id()).unwrap(),
            Probe::Held
        ));
    }
}
