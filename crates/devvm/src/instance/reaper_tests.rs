use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::ErrorKind;
use crate::backend::MachineRecord;
use crate::backend::fake::FakeBackend;
use crate::contain::VmProcess;
use crate::contain::contain_tests::Sleeper;
use crate::engine::engine_tests::dead_machine;
use crate::instance::dir::InstanceDir;
use crate::instance::locks::Probe;
use crate::instance::name::InstanceName;
use crate::instance::reaper::{Entries, ReapReport};
use crate::instance::record::{InstanceId, InstanceRecord, MarkerRead, Owner, RecordRead, read_marker};
use crate::instance::registry::registry_tests::{Fixture, create, fixture, fixture_with, lock_files};
use crate::store::layers::layers_tests::names;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::host::TestHost;
use crate::test_support::stub_backend::{Attach, StubBackend};

fn machine(sleeper: &Sleeper) -> MachineRecord {
    MachineRecord {
        process: sleeper.record(),
        qmp: String::new(),
        ssh: Vec::new(),
        serial_log: PathBuf::new(),
        leftovers: Vec::new(),
        no_reboot: false,
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
fn an_owned_instance_with_a_dead_owner_is_reaped_with_its_vm(#[fixture(unicode_home)] home: &Path) {
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
fn the_vm_of_a_dead_owner_is_found_and_killed_through_the_backend(#[fixture(unicode_home)] home: &Path) {
    let stub = StubBackend::new(Attach::Running);
    let fx = fixture_with(home, Some(stub.clone()));
    let socket = home.join("machine.sock");
    std::fs::write(&socket, b"x").unwrap();
    let owned = create(&fx, Some("foo"));
    set_machine(owned.dir(), dead_machine(vec![socket.clone()]));
    drop(owned);

    let report = fx.registry.reap();

    assert_eq!(report.reaped, ["foo"]);
    assert_eq!(stub.calls(), ["attach", "kill"]);
    assert!(!socket.exists());
}

#[skuld::test]
fn an_instance_whose_vm_cannot_be_killed_is_left_for_the_next_pass(#[fixture(unicode_home)] home: &Path) {
    let stub = StubBackend::new(Attach::Running);
    stub.fail_stops();
    let fx = fixture_with(home, Some(stub.clone()));
    let owned = create(&fx, Some("foo"));
    let machine = dead_machine(Vec::new());
    set_machine(owned.dir(), machine.clone());
    drop(owned);

    let report = fx.registry.reap();

    assert!(report.reaped.is_empty());
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(
        report.warnings[0].contains("foo") && report.warnings[0].contains("refuses to kill"),
        "{:?}",
        report.warnings
    );
    let dir = fx.registry.open(&InstanceName::parse("foo").unwrap()).unwrap();
    assert_eq!(
        dir.record().unwrap().machine,
        Some(machine),
        "the record still names the VM"
    );
}

#[skuld::test]
fn an_instance_whose_vm_cannot_be_looked_for_is_left_for_the_next_pass(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture_with(home, Some(StubBackend::new(Attach::Fails(ErrorKind::Io))));
    let owned = create(&fx, Some("foo"));
    set_machine(owned.dir(), dead_machine(Vec::new()));
    drop(owned);

    let report = fx.registry.reap();

    assert!(report.reaped.is_empty());
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("cannot check the VM of instance foo")),
        "{:?}",
        report.warnings
    );
    assert_eq!(names(&root(&fx)), ["foo"]);
}

#[skuld::test]
fn an_instance_with_a_recorded_vm_is_left_alone_when_the_build_has_no_backend(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture_with(home, None);
    let owned = create(&fx, Some("foo"));
    set_machine(owned.dir(), dead_machine(Vec::new()));
    drop(owned);

    let report = fx.registry.reap();

    assert!(report.reaped.is_empty());
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("foo") && w.contains("no VM backend")),
        "{:?}",
        report.warnings
    );
    assert_eq!(names(&root(&fx)), ["foo"]);
}

#[skuld::test]
fn a_live_owner_is_never_reaped(#[fixture(unicode_home)] home: &Path) {
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
fn an_ownerless_instance_is_never_reaped(#[fixture(unicode_home)] home: &Path) {
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
fn a_recorded_pid_with_another_start_token_is_not_killed(#[fixture(unicode_home)] home: &Path) {
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
fn a_dead_creators_new_dir_and_lock_files_are_removed(#[fixture(unicode_home)] home: &Path) {
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
fn a_new_dir_placed_after_the_listing_keeps_its_lock_files(#[fixture(unicode_home)] home: &Path) {
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
fn a_new_dir_with_no_lock_file_is_removed(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let new_dir = root(&fx).join(format!(".new-{}", InstanceId::random()));
    std::fs::create_dir_all(&new_dir).unwrap();
    fx.registry.reap();
    assert!(!new_dir.exists());
}

#[skuld::test]
fn an_orphan_lock_file_is_swept_but_an_ownerless_instances_locks_are_not(#[fixture(unicode_home)] home: &Path) {
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
fn the_lock_sweep_lists_the_instance_directories_once_however_many_locks_there_are(
    #[fixture(unicode_home)] home: &Path,
) {
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
fn a_dead_dir_is_deleted(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let dead = root(&fx).join(format!(".dead-{}", InstanceId::random()));
    std::fs::create_dir_all(dead.join("sub")).unwrap();
    std::fs::write(dead.join("sub").join("disk.img"), "x").unwrap();
    fx.registry.reap();
    assert!(!dead.exists());
}

#[skuld::test]
fn corrupt_record_missing_marker_and_torn_marker_are_quarantined_not_deleted(#[fixture(unicode_home)] home: &Path) {
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
fn a_corrupt_record_under_a_held_owner_lock_is_not_moved(#[fixture(unicode_home)] home: &Path) {
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
fn a_record_or_marker_from_a_newer_devvm_is_left_alone_with_a_warning(#[fixture(unicode_home)] home: &Path) {
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
fn an_op_lock_held_defers_the_reap_without_a_warning(#[fixture(unicode_home)] home: &Path) {
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
fn an_owned_instance_whose_lock_files_are_gone_is_still_reaped(#[fixture(unicode_home)] home: &Path) {
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
fn an_ownerless_instance_whose_lock_files_are_gone_gets_them_back(#[fixture(unicode_home)] home: &Path) {
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
fn create_in_a_loop_while_a_reaper_loop_runs_never_loses_a_live_instance(#[fixture(unicode_home)] home: &Path) {
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

#[skuld::test]
fn reaping_deletes_the_leftovers_of_the_killed_machine(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let sleeper = Sleeper::spawn();
    let (socket, lock) = (home.join("machine.sock"), home.join("machine.sock.lock"));
    std::fs::write(&socket, b"x").unwrap();
    std::fs::write(&lock, b"x").unwrap();
    let owned = create(&fx, Some("foo"));
    set_machine(
        owned.dir(),
        MachineRecord {
            leftovers: vec![socket.clone(), home.join("never-existed"), lock.clone()],
            ..machine(&sleeper)
        },
    );
    drop(owned);

    let report = fx.registry.reap();
    assert_eq!(report.reaped, ["foo"]);
    assert!(
        report.warnings.is_empty(),
        "an absent leftover is fine: {:?}",
        report.warnings
    );
    assert!(!socket.exists() && !lock.exists());
}

#[skuld::test]
fn the_leftovers_of_a_machine_that_already_exited_are_deleted(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let sleeper = Sleeper::spawn();
    let socket = home.join("machine.sock");
    std::fs::write(&socket, b"x").unwrap();
    let record = MachineRecord {
        leftovers: vec![socket.clone()],
        ..machine(&sleeper)
    };
    sleeper.finish();
    let owned = create(&fx, Some("foo"));
    set_machine(owned.dir(), record);
    drop(owned);

    let report = fx.registry.reap();
    assert_eq!(report.reaped, ["foo"]);
    assert!(!socket.exists());
}

#[skuld::test]
fn a_leftover_that_cannot_be_deleted_is_a_warning_and_the_instance_still_goes(#[fixture(unicode_home)] home: &Path) {
    // A backend that does not clean up after a kill: the reaper's own step meets the stuck file.
    let fx = fixture_with(
        home,
        Some(Arc::new(FakeBackend::new(&TestHost).with_leftovers_in(home, false))),
    );
    let sleeper = Sleeper::spawn();
    let (stuck, socket) = (home.join("stuck"), home.join("machine.sock"));
    std::fs::create_dir(&stuck).unwrap();
    std::fs::write(&socket, b"x").unwrap();
    let owned = create(&fx, Some("foo"));
    set_machine(
        owned.dir(),
        MachineRecord {
            leftovers: vec![stuck.clone(), socket.clone()],
            ..machine(&sleeper)
        },
    );
    drop(owned);

    let report = fx.registry.reap();
    assert_eq!(report.reaped, ["foo"]);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("stuck"), "{:?}", report.warnings);
    assert!(!socket.exists(), "the other leftover is still deleted");
}

#[skuld::test]
fn a_live_owners_machine_keeps_its_leftovers(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let sleeper = Sleeper::spawn();
    let socket = home.join("machine.sock");
    std::fs::write(&socket, b"x").unwrap();
    let owned = create(&fx, Some("foo"));
    set_machine(
        owned.dir(),
        MachineRecord {
            leftovers: vec![socket.clone()],
            ..machine(&sleeper)
        },
    );

    fx.registry.reap();
    assert!(socket.exists());
    drop(owned);
}

#[skuld::test]
fn a_new_directory_whose_name_is_not_an_id_is_reported_and_left_alone(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let stray = root(&fx).join(".new-not-an-id");
    std::fs::create_dir_all(&stray).unwrap();

    let report = fx.registry.reap();

    assert!(
        stray.is_dir(),
        "a directory the reaper cannot attribute is never deleted"
    );
    assert!(
        report.warnings.iter().any(|w| w.contains(".new-not-an-id")),
        "it is not silently ignored: {:?}",
        report.warnings
    );
}

#[skuld::test]
fn an_entry_whose_type_cannot_be_read_is_warned_about_and_blocks_the_lock_sweep(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let mut entries = Entries::default();
    entries.push(
        "foo".into(),
        root(&fx).join("foo"),
        Err(std::io::Error::other("the type is unreadable")),
    );
    let mut report = ReapReport::default();

    fx.registry.reap_listed(entries, &mut report);

    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("foo") && w.contains("the type is unreadable")),
        "{:?}",
        report.warnings
    );
    let mut entries = Entries::default();
    entries.push(
        "foo".into(),
        root(&fx).join("foo"),
        Err(std::io::Error::other("the type is unreadable")),
    );
    let (_, unattributed) = fx.registry.attribute(entries);
    assert!(unattributed, "a lock file may belong to the entry nobody could examine");
}

#[skuld::test]
fn a_placed_directory_nobody_can_attribute_keeps_every_lock_file(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let orphan = InstanceId::random();
    drop(fx.registry.locks().acquire_owner_new(&orphan).unwrap());
    let stray = root(&fx).join("bar");
    std::fs::create_dir_all(&stray).unwrap();
    std::fs::write(stray.join("record.json"), "{ torn").unwrap();
    std::fs::write(stray.join("owner.json"), "{ torn").unwrap();

    // The directory may well be the instance that owns the lock files: neither of its files says which it is.
    let mut report = ReapReport::default();
    fx.registry.sweep_locks(&mut report);
    assert_eq!(lock_files(&fx).len(), 2, "{:?}", report.warnings);

    std::fs::remove_dir_all(&stray).unwrap();
    fx.registry.sweep_locks(&mut report);
    assert!(lock_files(&fx).is_empty(), "with nothing unattributed the orphan goes");
}

#[skuld::test]
fn an_instance_whose_record_turns_unreadable_after_the_op_lock_is_left_for_the_next_pass(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = fixture(home);
    drop(create(&fx, Some("foo")));
    let path = root(&fx).join("foo");
    let RecordRead::Valid(record) = InstanceRecord::read(&path).unwrap() else {
        panic!("the record should be readable before the reap");
    };
    let mut report = ReapReport::default();
    let mut tear = || std::fs::write(path.join("record.json"), "{ torn").unwrap();

    fx.registry
        .reap_owned_with("foo", &path, *record, &mut report, &mut tear);

    assert!(report.reaped.is_empty(), "{report:?}");
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    let warning = &report.warnings[0];
    assert!(
        warning.contains("cannot read instance foo") && warning.contains("unreadable"),
        "{warning}"
    );
    assert!(path.is_dir(), "an instance whose record cannot be read is not removed");
}
