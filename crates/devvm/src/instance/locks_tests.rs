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
        .try_owner_with(&id, &mut || {}, &mut || std::fs::remove_file(&path).unwrap())
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
            locks.remove(&id).unwrap();
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

    locks.remove(&first).unwrap();
    assert!(!locks.owner_path(&first).exists());
    assert!(!locks.op_path(&first).exists());
    assert_eq!(locks.ids().unwrap(), vec![second]);
    locks.remove(&first).unwrap();
}

#[skuld::test]
fn renaming_waits_for_a_reader_and_the_reader_never_waits_for_another_reader(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    drop(locks.acquire_owner_new(&InstanceId::random()).unwrap());
    let path = home.join("locks").join("instances.dir");
    let log = std::sync::Mutex::new(Vec::new());
    let (contended, contention) = std::sync::mpsc::channel();

    std::thread::scope(|scope| {
        let renamer = locks
            .reading(home, || {
                let mut other = FileLock::open(&path).unwrap();
                assert!(other.try_lock(Mode::Shared).unwrap(), "readers share");
                other.unlock().unwrap();
                let renamer = scope.spawn(|| {
                    locks
                        .excluding_readers(&mut || contended.send(()).unwrap(), || {
                            log.lock().unwrap().push("rename")
                        })
                        .unwrap();
                });
                // The renamer found the readers' lock taken: it waits until this closure returns.
                contention.recv().unwrap();
                log.lock().unwrap().push("reader done");
                renamer
            })
            .unwrap()
            .unwrap();
        renamer.join().unwrap();
    });
    assert_eq!(*log.lock().unwrap(), ["reader done", "rename"]);
}

#[skuld::test]
fn a_reader_cannot_start_while_a_directory_is_renamed(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    drop(locks.acquire_owner_new(&InstanceId::random()).unwrap());
    let path = home.join("locks").join("instances.dir");
    locks
        .excluding_readers(&mut || {}, || {
            let mut reader = FileLock::open(&path).unwrap();
            assert!(!reader.try_lock(Mode::Shared).unwrap());
        })
        .unwrap();
}

#[skuld::test]
fn reading_an_absent_directory_reads_nothing_and_creates_nothing(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    assert!(
        locks
            .reading(&home.join("instances").join("foo"), || 7)
            .unwrap()
            .is_none()
    );
    assert!(!home.join("locks").exists());
}

#[skuld::test]
fn a_reader_that_stalls_after_seeing_a_directory_still_reads_under_the_lock_of_a_first_create(
    #[fixture(anna_home)] home: &Path,
) {
    let locks = locks(home);
    let dir = home.join("instances").join("foo");
    std::fs::create_dir_all(&dir).unwrap();
    let lock_path = home.join("locks").join("instances.dir");
    let mut read_locked = None;
    let read = locks
        .reading_with(
            &dir,
            &mut || {
                // The first create of a fresh state, while the reader is stalled.
                drop(locks.acquire_owner_new(&InstanceId::random()).unwrap());
            },
            || {
                let mut renamer = FileLock::open(&lock_path).unwrap();
                read_locked = Some(!renamer.try_lock(Mode::Exclusive).unwrap());
            },
        )
        .unwrap();
    assert!(read.is_some());
    assert_eq!(read_locked, Some(true), "the reader read without holding instances.dir");
}

#[skuld::test]
fn a_reader_that_saw_a_directory_but_finds_no_lock_file_recreates_it_and_reads(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    let dir = home.join("instances").join("foo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("owner.json"), "x").unwrap();

    // The directory exists, so the lock file went missing from outside: the reader must not report "nothing there".
    assert_eq!(locks.reading(&dir, || 7).unwrap(), Some(7));
    assert!(home.join("locks").join("instances.dir").exists());
    assert!(
        !matches!(
            locks.read_marker(&dir).unwrap(),
            crate::instance::record::MarkerRead::Absent
        ),
        "an existing file is not absent"
    );
}

#[skuld::test]
fn probes_racing_the_unlink_of_the_lock_files_see_present_or_gone_never_an_error(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    let finished = std::sync::atomic::AtomicBool::new(false);
    let (probing, first_probe_done) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let prober = scope.spawn(|| {
            let mut first = true;
            loop {
                // Windows refuses an open that meets an unlink with "access denied", not "not found".
                locks.try_owner(&id).unwrap();
                locks.try_op_exclusive(&id).unwrap();
                if std::mem::take(&mut first) {
                    probing.send(()).unwrap();
                }
                if finished.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
            }
        });
        // The prober has probed once before the unlinking starts: it is running, not still to be scheduled.
        first_probe_done.recv().unwrap();
        for _ in 0..3000 {
            drop(locks.acquire_owner_new(&id).unwrap());
            locks.remove(&id).unwrap();
        }
        finished.store(true, std::sync::atomic::Ordering::Release);
        // A failed probe panics the prober, and the join reports it.
        prober.join().unwrap();
    });
}

#[skuld::test]
fn a_probe_waits_while_the_lock_files_are_unlinked_and_then_finds_them_gone(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    let locks = &locks;
    let id = InstanceId::random();
    drop(locks.acquire_owner_new(&id).unwrap());
    let (waiting, opener_waits) = std::sync::mpsc::channel();

    std::thread::scope(|scope| {
        let probe = locks
            .excluding_readers(&mut || {}, || {
                // `waiting` is dropped with the closure, so a probe that does not wait fails the `recv` below
                // instead of hanging it.
                let probe =
                    scope.spawn(move || locks.try_owner_with(&id, &mut || waiting.send(()).unwrap(), &mut || {}));
                assert!(
                    opener_waits.recv().is_ok(),
                    "the probe opened the lock file while it could be unlinked"
                );
                std::fs::remove_file(locks.owner_path(&id)).unwrap();
                std::fs::remove_file(locks.op_path(&id)).unwrap();
                probe
            })
            .unwrap();
        assert!(matches!(probe.join().unwrap().unwrap(), Probe::Gone));
    });
}

#[skuld::test]
fn unlinking_the_lock_files_waits_for_whoever_is_opening_one(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    let locks = &locks;
    let id = InstanceId::random();
    drop(locks.acquire_owner_new(&id).unwrap());
    let (waiting, remover_waits) = std::sync::mpsc::channel();

    std::thread::scope(|scope| {
        locks
            .reading(home, || {
                let remover = scope.spawn(move || locks.remove_with(&id, &mut || waiting.send(()).unwrap()));
                assert!(
                    remover_waits.recv().is_ok(),
                    "the lock files were unlinked while a probe could be opening them"
                );
                assert!(locks.owner_path(&id).is_file() && locks.op_path(&id).is_file());
                remover
            })
            .unwrap()
            .unwrap()
            .join()
            .unwrap()
            .unwrap();
    });
    assert!(!locks.owner_path(&id).exists() && !locks.op_path(&id).exists());
}

#[skuld::test]
fn a_failed_unlink_of_the_op_file_leaves_the_owner_file_for_the_sweep(#[fixture(anna_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    drop(locks.acquire_owner_new(&id).unwrap());
    let op = locks.op_path(&id);

    let error = locks
        .remove_via(&id, &mut || {}, &|path| {
            if path == op {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            } else {
                std::fs::remove_file(path)
            }
        })
        .expect_err("an unlink that fails is reported, with its cause");
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(
        error.message().contains(&op.display().to_string()),
        "the error names the file: {}",
        error.message()
    );
    assert!(
        error.message().to_lowercase().contains("permission denied"),
        "the error keeps the cause: {}",
        error.message()
    );

    assert!(locks.op_path(&id).exists());
    assert!(
        locks.owner_path(&id).exists(),
        "the sweep would never see this id as free"
    );
    // The sweep's view: the owner file is there to be taken, and the next remove finishes the job.
    assert!(matches!(locks.try_owner(&id).unwrap(), Probe::Free(_)));
    locks.remove(&id).unwrap();
    assert!(!locks.op_path(&id).exists() && !locks.owner_path(&id).exists());
}
