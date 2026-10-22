use std::path::Path;
use std::sync::mpsc::Receiver;

use crate::backend::MachineRecord;
use crate::contain::VmProcess;
use crate::contain::contain_tests::Sleeper;
use crate::engine::engine_tests::{engine_fixture, instance_in};
use crate::instance::name::InstanceName;
use crate::instance::record::InstanceRecord;
use crate::store::layers::layers_tests::names;
use crate::test_support::fixtures::{ChannelProgress, MemoryProgress, anna_home};
use crate::{ErrorKind, Instance};

/// Reads progress messages until `count` of them say `waiting for`: that many commands are provably queued on a lock.
fn queued(messages: &Receiver<String>, count: usize) {
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
    assert!(names(&fx.env.dirs.instance_locks()).is_empty());
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
    assert!(names(&fx.env.dirs.instance_locks()).is_empty());
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
        fx.engine.registry().locks().remove(first.id());
        let second = instance_in(&fx, "foo", home);
        let before = std::fs::read(second.path().join("record.json")).unwrap();
        drop(holder);

        let error = waiter.join().unwrap().expect_err("the instance it queued for is gone");
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(std::fs::read(second.path().join("record.json")).unwrap(), before);
        assert!(fx.engine.registry().open(&InstanceName::parse("foo").unwrap()).is_ok());
    });
}
