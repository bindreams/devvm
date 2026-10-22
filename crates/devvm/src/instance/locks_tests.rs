use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Stdio;

use crate::ErrorKind;
use crate::exit::NullProgress;
use crate::instance::locks::{Locks, Probe};
use crate::instance::record::InstanceId;
use crate::store::lock::{FileLock, Mode};
use crate::test_support::fixtures::{ChannelProgress, anna_home};
use crate::test_support::helper;

fn locks(home: &Path) -> Locks {
    Locks::new(home.join("locks"))
}

#[skuld::test]
fn acquire_creates_both_files_and_holds_the_owner_lock(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    let owner = locks.acquire_owner_new(&id).unwrap();
    assert!(locks.owner_path(&id).is_file());
    assert!(locks.op_path(&id).is_file());
    assert!(matches!(locks.try_owner(&id).unwrap(), Probe::Held));
    drop(owner);
    assert!(matches!(locks.try_owner(&id).unwrap(), Probe::Free(_)));
}

#[skuld::test]
fn owner_held_by_another_process_reads_held_and_free_after_it_dies(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    drop(locks.acquire_owner_new(&id).unwrap());

    let path = locks.owner_path(&id);
    let mut child = helper::command("hold-lock", &[path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "held");
    assert!(matches!(locks.try_owner(&id).unwrap(), Probe::Held));

    child.kill().unwrap();
    child.wait().unwrap();
    // The OS releases a dead holder's lock asynchronously; this blocking take is the event awaited.
    let mut waiter = FileLock::open(&path).unwrap();
    waiter
        .lock(Mode::Exclusive, "the killed holder", &NullProgress)
        .unwrap();
    drop(waiter);
    assert!(matches!(locks.try_owner(&id).unwrap(), Probe::Free(_)));
}

#[skuld::test]
fn try_owner_never_creates_the_file(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    assert!(matches!(locks.try_owner(&id).unwrap(), Probe::Gone));
    assert!(matches!(locks.try_op_exclusive(&id).unwrap(), Probe::Gone));
    assert!(
        !home.join("locks").exists(),
        "probing must not create the lock directory either"
    );
}

#[skuld::test]
fn try_owner_on_a_file_unlinked_after_the_open_is_gone(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    drop(locks.acquire_owner_new(&id).unwrap());
    let path = locks.owner_path(&id);
    let probe = locks
        .try_owner_with(&id, &mut || std::fs::remove_file(&path).unwrap())
        .unwrap();
    assert!(
        matches!(probe, Probe::Gone),
        "a lock on an unlinked file proves nothing"
    );
}

#[skuld::test]
fn acquire_retries_when_the_file_is_unlinked_before_verification(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    let path = locks.owner_path(&id);
    let mut calls = 0;
    let owner = locks
        .acquire_owner_new_with(&id, &mut || {
            calls += 1;
            if calls == 1 {
                std::fs::remove_file(&path).unwrap();
            }
        })
        .unwrap();
    assert_eq!(calls, 2, "the sweeper's progress makes the creator start over");
    assert!(path.is_file());
    assert!(locks.op_path(&id).is_file());
    assert!(matches!(locks.try_owner(&id).unwrap(), Probe::Held));
    drop(owner);
}

#[skuld::test]
fn op_exclusive_waits_for_the_holder_and_says_so_first(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    drop(locks.acquire_owner_new(&id).unwrap());
    let holder = locks.op_exclusive(&id, "holder", &NullProgress).unwrap();
    let (progress, messages) = ChannelProgress::new();
    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| locks.op_exclusive(&id, "the busy instance", &*progress).unwrap());
        assert_eq!(messages.recv().unwrap(), "waiting for the busy instance");
        assert!(!waiter.is_finished());
        drop(holder);
        drop(waiter.join().unwrap());
    });
}

#[skuld::test]
fn op_wait_that_wakes_on_an_unlinked_file_is_not_found(#[fixture(anna_home)] home: &Path) {
    for shared in [false, true] {
        let locks = locks(home);
        let id = InstanceId::random();
        drop(locks.acquire_owner_new(&id).unwrap());
        let holder = locks.op_exclusive(&id, "holder", &NullProgress).unwrap();
        let (progress, messages) = ChannelProgress::new();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                if shared {
                    locks.op_shared(&id, "the instance", &*progress).map(drop)
                } else {
                    locks.op_exclusive(&id, "the instance", &*progress).map(drop)
                }
            });
            assert_eq!(messages.recv().unwrap(), "waiting for the instance");
            // The holder removes the instance (unlinking the lock files) and then releases.
            locks.remove(&id);
            drop(holder);
            let error = waiter.join().unwrap().expect_err("the instance is gone");
            assert_eq!(error.kind(), ErrorKind::NotFound);
            assert!(error.message().contains("gone"), "{}", error.message());
        });
    }
}

#[skuld::test]
fn op_on_a_missing_instance_is_not_found(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    assert_eq!(
        locks.op_exclusive(&id, "x", &NullProgress).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        locks.op_shared(&id, "x", &NullProgress).unwrap_err().kind(),
        ErrorKind::NotFound
    );
}

#[skuld::test]
fn op_shared_holders_coexist_and_exclude_exclusive(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    drop(locks.acquire_owner_new(&id).unwrap());
    let first = locks.op_shared(&id, "a", &NullProgress).unwrap();
    let second = locks.op_shared(&id, "b", &NullProgress).unwrap();
    assert!(matches!(locks.try_op_exclusive(&id).unwrap(), Probe::Held));
    drop(first);
    assert!(matches!(locks.try_op_exclusive(&id).unwrap(), Probe::Held));
    drop(second);
    assert!(matches!(locks.try_op_exclusive(&id).unwrap(), Probe::Free(_)));
}

#[skuld::test]
fn remove_deletes_both_files_and_ids_lists_only_present_ones(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    assert!(locks.ids().unwrap().is_empty(), "no directory means no ids");
    let (first, second) = (InstanceId::random(), InstanceId::random());
    drop(locks.acquire_owner_new(&first).unwrap());
    drop(locks.acquire_owner_new(&second).unwrap());
    std::fs::write(home.join("locks").join("unrelated.txt"), "x").unwrap();
    let mut listed = locks.ids().unwrap();
    listed.sort_by_key(|id| id.to_string());
    let mut expected = vec![first, second];
    expected.sort_by_key(|id| id.to_string());
    assert_eq!(listed, expected);

    locks.remove(&first);
    assert!(!locks.owner_path(&first).exists());
    assert!(!locks.op_path(&first).exists());
    assert_eq!(locks.ids().unwrap(), vec![second]);
    locks.remove(&first);
}
