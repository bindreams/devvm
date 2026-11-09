use std::cell::RefCell;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::is_there;
use crate::ErrorKind;
use crate::exit::NullProgress;
use crate::host::{self, Comparison};
use crate::instance::locks::{Locks, Probe};
use crate::instance::record::InstanceId;
use crate::store::lock::{FileLock, Mode};
use crate::test_groups::{Group, symlinks};
use crate::test_support::fixtures::{ChannelProgress, unicode_home};
use crate::test_support::helper;
use crate::test_support::host::RecordingHost;
use crate::test_support::name_change::NameChange;
use crate::test_support::symlink::symlink;

fn locks(home: &Path) -> Locks {
    Locks::new(home.join("locks"), host::current())
}

#[skuld::test]
fn acquire_creates_both_files_and_holds_the_owner_lock(#[fixture(unicode_home)] home: &Path) {
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
fn owner_held_by_another_process_reads_held_and_free_after_it_dies(#[fixture(unicode_home)] home: &Path) {
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
    waiter.lock(Mode::Exclusive, "the killed holder", &mut |_| {}).unwrap();
    drop(waiter);
    assert!(matches!(locks.try_owner(&id).unwrap(), Probe::Free(_)));
}

#[skuld::test]
fn try_owner_never_creates_the_file(#[fixture(unicode_home)] home: &Path) {
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
fn try_owner_on_a_file_unlinked_after_the_open_is_gone(#[fixture(unicode_home)] home: &Path) {
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
fn acquire_retries_when_the_file_is_unlinked_before_verification(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    let path = locks.owner_path(&id);
    let mut calls = 0;
    let owner = locks
        .acquire_owner_new_with(
            &id,
            &mut || {
                calls += 1;
                if calls == 1 {
                    std::fs::remove_file(&path).unwrap();
                }
            },
            &mut || {},
        )
        .unwrap();
    assert_eq!(calls, 2, "the sweeper's progress makes the creator start over");
    assert!(path.is_file());
    assert!(locks.op_path(&id).is_file());
    assert!(matches!(locks.try_owner(&id).unwrap(), Probe::Held));
    drop(owner);
}

#[skuld::test]
fn acquire_retries_when_another_file_is_at_the_name_before_verification(#[fixture(unicode_home)] home: &Path) {
    for change in NameChange::ALL {
        let locks = locks(home);
        let id = InstanceId::random();
        let path = locks.owner_path(&id);
        let mut calls = 0;
        let owner = locks
            .acquire_owner_new_with(
                &id,
                &mut || {
                    calls += 1;
                    if calls == 1 {
                        change.apply(&path);
                    }
                },
                &mut || {},
            )
            .unwrap();
        assert_eq!(calls, 2, "{change:?}: a file at the name is not the locked file");
        assert!(
            matches!(locks.try_owner(&id).unwrap(), Probe::Held),
            "{change:?}: the lock is on the file at the name"
        );
        drop(owner);
        locks.remove(&id).unwrap();
    }
}

#[skuld::test]
fn try_owner_on_a_file_replaced_after_the_open_opens_the_file_at_the_name(#[fixture(unicode_home)] home: &Path) {
    for change in NameChange::ALL {
        let locks = locks(home);
        let id = InstanceId::random();
        drop(locks.acquire_owner_new(&id).unwrap());
        let path = locks.owner_path(&id);
        let mut looks = 0;
        let probe = locks
            .try_owner_with(&id, &mut || {}, &mut || {
                looks += 1;
                if looks == 1 {
                    change.apply(&path);
                }
            })
            .unwrap();
        assert_eq!(
            looks, 2,
            "{change:?}: a file at the name is not gone, the probe opens it"
        );
        let Probe::Free(lock) = probe else {
            panic!("{change:?}: nobody holds the file at the name");
        };
        assert_eq!(lock.is_at_path().unwrap(), Comparison::Same, "{change:?}");
        drop(lock);
        locks.remove(&id).unwrap();
    }
}

#[skuld::test]
fn try_owner_on_a_replaced_file_that_is_held_reads_held(#[fixture(unicode_home)] home: &Path) {
    for change in NameChange::ALL {
        let locks = locks(home);
        let id = InstanceId::random();
        drop(locks.acquire_owner_new(&id).unwrap());
        let path = locks.owner_path(&id);
        let mut holder = None;
        let mut looks = 0;
        let probe = locks
            .try_owner_with(&id, &mut || {}, &mut || {
                looks += 1;
                if looks == 1 {
                    change.apply(&path);
                    let mut other = FileLock::open(&path).unwrap();
                    assert!(other.try_lock(Mode::Exclusive).unwrap());
                    holder = Some(other);
                }
            })
            .unwrap();
        assert!(matches!(probe, Probe::Held), "{change:?}: the file at the name is held");
        drop(holder);
        locks.remove(&id).unwrap();
    }
}

#[skuld::test]
fn a_probe_holds_instances_dir_shared_from_the_open_to_the_look(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    drop(locks.acquire_owner_new(&id).unwrap());
    let dir_path = home.join("locks").join("instances.dir");
    let mut unlinker_blocked = None;
    let probe = locks
        .try_owner_with(&id, &mut || {}, &mut || {
            let mut unlinker = FileLock::open(&dir_path).unwrap();
            unlinker_blocked = Some(!unlinker.try_lock(Mode::Exclusive).unwrap());
        })
        .unwrap();
    assert!(matches!(probe, Probe::Free(_)));
    assert_eq!(
        unlinker_blocked,
        Some(true),
        "an unlinker could run between the open and the look"
    );
}

#[skuld::test]
fn acquire_puts_back_a_deleted_instances_dir_and_keeps_the_file_it_locked(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    let dir_path = home.join("locks").join("instances.dir");
    let mut calls = 0;
    let owner = locks
        .acquire_owner_new_with(
            &id,
            &mut || {
                calls += 1;
                if calls == 1 {
                    std::fs::remove_file(&dir_path).unwrap();
                }
            },
            &mut || {},
        )
        .unwrap();
    assert_eq!(calls, 1, "the lock file is still the one at its name");
    assert!(dir_path.is_file(), "the lock file is put back");
    drop(owner);
}

#[skuld::test]
fn a_look_at_the_directory_that_fails_is_the_error_and_creates_nothing(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    // A NUL in a path is an error of the look, whatever the platform.
    let error = locks.reading(Path::new("a\0b"), || 7).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(!home.join("locks").exists());
}

#[skuld::test]
fn op_wait_that_wakes_on_a_replaced_file_takes_the_file_at_the_name(#[fixture(unicode_home)] home: &Path) {
    for change in NameChange::ALL {
        for shared in [false, true] {
            let locks = locks(home);
            let id = InstanceId::random();
            drop(locks.acquire_owner_new(&id).unwrap());
            let holder = locks.op_exclusive(&id, "holder", &NullProgress).unwrap();
            let (progress, messages) = ChannelProgress::new();
            std::thread::scope(|scope| {
                let waiter = scope.spawn(|| {
                    if shared {
                        locks.op_shared(&id, "the instance", &*progress)
                    } else {
                        locks.op_exclusive(&id, "the instance", &*progress)
                    }
                });
                assert_eq!(messages.recv().unwrap(), "waiting for the instance");
                change.apply(&locks.op_path(&id));
                drop(holder);
                let lock = waiter
                    .join()
                    .unwrap()
                    .expect("a file is at the name: the instance is not gone");
                assert_eq!(
                    lock.is_at_path().unwrap(),
                    Comparison::Same,
                    "{change:?}, shared {shared}"
                );
            });
            locks.remove(&id).unwrap();
        }
    }
}

#[skuld::test]
fn a_link_at_a_lock_name_is_an_error_naming_the_file_held_or_free_target_alike(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(symlinks)] _group: &Group,
) {
    let locks = locks(home);
    let id = InstanceId::random();
    drop(locks.acquire_owner_new(&id).unwrap());
    let (owner, op) = (locks.owner_path(&id), locks.op_path(&id));
    let mut targets = Vec::new();
    for path in [&owner, &op] {
        let mut target = path.clone().into_os_string();
        target.push(".target");
        let target = std::path::PathBuf::from(target);
        std::fs::rename(path, &target).unwrap();
        symlink(&target, path);
        targets.push(target);
    }
    let named = |error: crate::Error, path: &Path| {
        assert!(error.message().contains(&path.display().to_string()), "{error}");
        assert!(error.message().contains("link"), "{error}");
    };
    let check = || {
        named(locks.acquire_owner_new(&id).map(drop).unwrap_err(), &owner);
        named(locks.try_owner_recreating(&id).map(drop).unwrap_err(), &owner);
        named(locks.try_owner(&id).map(drop).unwrap_err(), &owner);
        named(locks.try_op_exclusive(&id).map(drop).unwrap_err(), &op);
        named(locks.op_shared(&id, "x", &NullProgress).map(drop).unwrap_err(), &op);
        named(locks.op_exclusive(&id, "x", &NullProgress).map(drop).unwrap_err(), &op);
    };
    // Held by another open in this process: the link is still the answer, and no take waits for the target.
    let held: Vec<FileLock> = targets
        .iter()
        .map(|target| {
            let mut lock = FileLock::open(target).unwrap();
            assert!(lock.try_lock(Mode::Exclusive).unwrap());
            lock
        })
        .collect();
    check();
    drop(held);
    check();
}

#[skuld::test]
fn op_exclusive_waits_for_the_holder_and_says_so_first(#[fixture(unicode_home)] home: &Path) {
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
fn an_op_wait_after_a_wait_for_instances_dir_still_says_what_it_waits_for(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    let locks = &locks;
    let id = InstanceId::random();
    drop(locks.acquire_owner_new(&id).unwrap());
    let holder = locks.op_exclusive(&id, "holder", &NullProgress).unwrap();
    let (progress, messages) = ChannelProgress::new();
    let (held, dir_held) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (waiting, dir_waits) = std::sync::mpsc::channel();

    std::thread::scope(|scope| {
        let excluder = scope.spawn(move || {
            locks
                .excluding_readers(&mut || {}, || {
                    held.send(()).unwrap();
                    released.recv().unwrap();
                })
                .unwrap();
        });
        dir_held.recv().unwrap();
        let waiter = scope.spawn(|| {
            locks
                .op_with(
                    &id,
                    Mode::Exclusive,
                    "the instance",
                    &*progress,
                    &mut || waiting.send(()).unwrap(),
                    &mut || {},
                )
                .unwrap()
        });
        // The waiter found `instances.dir` taken; the op lock is held too, so it waits for that next.
        dir_waits.recv().unwrap();
        release.send(()).unwrap();
        assert_eq!(messages.recv().unwrap(), "waiting for the instance");
        excluder.join().unwrap();
        drop(holder);
        drop(waiter.join().unwrap());
    });
}

/// The first try of the take wins and its look restarts it; the second wait ends as it starts. The wait is still said
/// and still runs `before_wait`, once.
#[skuld::test]
fn a_wait_after_a_restart_says_what_it_waits_for_and_runs_before_wait(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    drop(locks.acquire_owner_new(&id).unwrap());
    let path = locks.op_path(&id);
    let (progress, messages) = ChannelProgress::new();
    let blocker = RefCell::new(None);
    let (mut waits, mut looks) = (0, 0);
    let lock = locks
        .op_with(
            &id,
            Mode::Exclusive,
            "the instance",
            &*progress,
            // The blocker goes as the wait starts, so the lock is won on the retry.
            &mut || {
                waits += 1;
                blocker.take();
            },
            &mut || {
                looks += 1;
                if looks == 1 {
                    NameChange::UnlinkThenRecreate.apply(&path);
                    let mut held = FileLock::open(&path).unwrap();
                    assert!(held.try_lock(Mode::Exclusive).unwrap());
                    *blocker.borrow_mut() = Some(held);
                }
            },
        )
        .unwrap();
    assert_eq!(lock.is_at_path().unwrap(), Comparison::Same);
    assert_eq!((waits, looks), (1, 2));
    assert_eq!(messages.try_recv().unwrap(), "waiting for the instance");
    assert!(messages.try_recv().is_err(), "said once");
}

#[skuld::test]
fn op_wait_that_wakes_on_an_unlinked_file_is_not_found(#[fixture(unicode_home)] home: &Path) {
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
fn op_on_a_missing_instance_is_not_found(#[fixture(unicode_home)] home: &Path) {
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
fn op_shared_holders_coexist_and_exclude_exclusive(#[fixture(unicode_home)] home: &Path) {
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
fn remove_deletes_both_files_and_ids_lists_only_present_ones(#[fixture(unicode_home)] home: &Path) {
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
fn renaming_waits_for_a_reader_and_the_reader_never_waits_for_another_reader(#[fixture(unicode_home)] home: &Path) {
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
fn a_reader_cannot_start_while_a_directory_is_renamed(#[fixture(unicode_home)] home: &Path) {
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
fn reading_an_absent_directory_reads_nothing_and_creates_nothing(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    assert!(
        locks
            .reading(&home.join("instances").join("foo"), || 7)
            .unwrap()
            .is_none()
    );
    assert!(!home.join("locks").exists());
}

/// A look at a name that is not there is `FileId::compare`'s absent: gone, below a file, or a name the host cannot hold.
#[skuld::test]
fn reading_a_directory_that_is_not_there_reads_nothing_whatever_makes_it_absent(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    std::fs::write(home.join("file"), b"x").unwrap();
    let absent = [home.join("gone"), home.join("file").join("below"), home.join("a|b")];
    for seen in &absent {
        assert!(locks.reading(seen, || 7).unwrap().is_none(), "{}", seen.display());
    }
    assert!(!home.join("locks").exists());

    drop(locks.acquire_owner_new(&InstanceId::random()).unwrap());
    for seen in &absent {
        assert!(locks.reading(seen, || 7).unwrap().is_none(), "{}", seen.display());
    }
}

/// A link at the name is something there, followed or not: `reading` does not follow it.
#[skuld::test]
fn reading_a_directory_that_is_a_link_reads_it_even_when_the_link_dangles(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(symlinks)] _group: &Group,
) {
    let locks = locks(home);
    let (real, to_dir, dangling) = (home.join("real"), home.join("to_dir"), home.join("dangling"));
    std::fs::create_dir(&real).unwrap();
    symlink(&real, &to_dir);
    symlink(&home.join("nowhere"), &dangling);
    for seen in [&to_dir, &dangling] {
        let mut seen_first = false;
        let read = locks.reading_with(seen, &mut || seen_first = true, || 7).unwrap();
        assert_eq!(read, Some(7), "{}", seen.display());
        assert!(
            seen_first,
            "{}: found while instances.dir did not exist",
            seen.display()
        );
        std::fs::remove_dir_all(home.join("locks")).unwrap();
    }
    drop(locks.acquire_owner_new(&InstanceId::random()).unwrap());
    for seen in [&to_dir, &dangling] {
        assert_eq!(locks.reading(seen, || 7).unwrap(), Some(7), "{}", seen.display());
    }
}

#[skuld::test]
fn a_reader_that_stalls_after_seeing_a_directory_still_reads_under_the_lock_of_a_first_create(
    #[fixture(unicode_home)] home: &Path,
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
fn a_reader_that_saw_a_directory_but_finds_no_lock_file_recreates_it_and_reads(#[fixture(unicode_home)] home: &Path) {
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
fn probes_racing_the_unlink_of_the_lock_files_see_present_or_gone_never_an_error(#[fixture(unicode_home)] home: &Path) {
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
fn a_probe_waits_while_the_lock_files_are_unlinked_and_then_finds_them_gone(#[fixture(unicode_home)] home: &Path) {
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
fn unlinking_the_lock_files_waits_for_whoever_is_opening_one(#[fixture(unicode_home)] home: &Path) {
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
fn a_failed_unlink_of_the_op_file_leaves_the_owner_file_for_the_sweep(#[fixture(unicode_home)] home: &Path) {
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

fn without_posix_unlink(dir: PathBuf) -> Locks {
    Locks::new(dir, Box::leak(Box::new(RecordingHost::without_posix_unlink())))
}

fn assert_names_the_volume_requirement(error: crate::Error, home: &Path) {
    assert_eq!(error.kind(), ErrorKind::NotSupported, "{error}");
    assert!(error.message().contains("NTFS or ReFS"), "{}", error.message());
    assert!(
        error.message().contains(&home.join("locks").display().to_string()),
        "{}",
        error.message()
    );
}

#[skuld::test]
fn a_lock_take_on_a_volume_without_posix_unlink_is_not_supported_and_names_the_requirement(
    #[fixture(unicode_home)] home: &Path,
) {
    let locks = without_posix_unlink(home.join("locks"));
    let id = InstanceId::random();

    let errors = [
        locks.acquire_owner_new(&id).map(drop).unwrap_err(),
        locks.try_owner_recreating(&id).map(drop).unwrap_err(),
        locks.excluding_readers(&mut || {}, || ()).unwrap_err(),
    ];
    for error in errors {
        assert_names_the_volume_requirement(error, home);
    }
}

/// The directory is made on a good volume, so no entry point reaches the check by making it.
#[skuld::test]
fn every_entry_point_checks_the_volume_of_an_existing_instances_dir(#[fixture(unicode_home)] home: &Path) {
    let id = InstanceId::random();
    drop(locks(home).acquire_owner_new(&id).unwrap());
    let locks = without_posix_unlink(home.join("locks"));

    let errors = [
        locks.try_owner(&id).map(drop).unwrap_err(),
        locks.try_op_exclusive(&id).map(drop).unwrap_err(),
        locks.op_shared(&id, "x", &NullProgress).map(drop).unwrap_err(),
        locks.op_exclusive(&id, "x", &NullProgress).map(drop).unwrap_err(),
        locks.reading(home, || ()).unwrap_err(),
        locks.excluding_readers(&mut || {}, || ()).unwrap_err(),
        locks.remove(&id).unwrap_err(),
    ];
    for error in errors {
        assert_names_the_volume_requirement(error, home);
    }
}

#[skuld::test]
fn try_owner_recreating_makes_the_missing_files_and_holds_the_owner_lock(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    let Probe::Free(lock) = locks.try_owner_recreating(&id).unwrap() else {
        panic!("nobody holds a file that was not there");
    };
    assert_eq!(lock.is_at_path().unwrap(), Comparison::Same);
    assert!(locks.owner_path(&id).is_file());
    assert!(locks.op_path(&id).is_file(), "the op file is made with the owner file");
    drop(lock);
    locks.remove(&id).unwrap();

    drop(locks.acquire_owner_new(&id).unwrap());
    std::fs::remove_file(locks.op_path(&id)).unwrap();
    assert!(matches!(locks.try_owner_recreating(&id).unwrap(), Probe::Free(_)));
    assert!(locks.op_path(&id).is_file(), "a missing op file is put back");
}

#[skuld::test]
fn try_owner_recreating_starts_over_when_another_file_is_at_the_name_before_the_try(
    #[fixture(unicode_home)] home: &Path,
) {
    for change in NameChange::ALL {
        let locks = locks(home);
        let id = InstanceId::random();
        drop(locks.acquire_owner_new(&id).unwrap());
        let path = locks.owner_path(&id);
        let mut looks = 0;
        let probe = locks
            .try_owner_recreating_with(
                &id,
                &mut || {
                    looks += 1;
                    if looks == 1 {
                        change.apply(&path);
                    }
                },
                &mut || {},
            )
            .unwrap();
        assert_eq!(looks, 2, "{change:?}: a file at the name is not the file opened");
        let Probe::Free(lock) = probe else {
            panic!("{change:?}: nobody holds the file at the name");
        };
        assert_eq!(lock.is_at_path().unwrap(), Comparison::Same, "{change:?}");
        drop(lock);
        locks.remove(&id).unwrap();
    }
}

#[skuld::test]
fn try_owner_recreating_makes_the_file_again_when_it_is_unlinked_before_the_try(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    drop(locks.acquire_owner_new(&id).unwrap());
    let path = locks.owner_path(&id);
    let mut looks = 0;
    let probe = locks
        .try_owner_recreating_with(
            &id,
            &mut || {
                looks += 1;
                if looks == 1 {
                    std::fs::remove_file(&path).unwrap();
                }
            },
            &mut || {},
        )
        .unwrap();
    assert_eq!(looks, 2);
    let Probe::Free(lock) = probe else {
        panic!("nobody holds the file made again");
    };
    assert_eq!(lock.is_at_path().unwrap(), Comparison::Same);
    assert!(path.is_file());
}

/// Whether `instances.dir` is held shared right now: an exclusive take is refused and a shared one admitted.
fn instances_dir_is_held_shared(locks_dir: &Path) -> bool {
    let mut other = FileLock::open(&locks_dir.join("instances.dir")).unwrap();
    let refused = !other.try_lock(Mode::Exclusive).unwrap();
    refused && other.try_lock(Mode::Shared).unwrap()
}

#[skuld::test]
fn an_acquire_compares_the_file_at_the_name_under_a_shared_hold_of_instances_dir(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    let mut held = None;
    drop(
        locks
            .acquire_owner_new_with(&InstanceId::random(), &mut || {}, &mut || {
                held = Some(instances_dir_is_held_shared(&home.join("locks")));
            })
            .unwrap(),
    );
    assert_eq!(held, Some(true));
}

#[skuld::test]
fn an_op_take_compares_the_file_at_the_name_under_a_shared_hold_of_instances_dir(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    drop(locks.acquire_owner_new(&id).unwrap());
    for mode in [Mode::Shared, Mode::Exclusive] {
        let mut held = None;
        let lock = locks
            .op_with(&id, mode, "x", &NullProgress, &mut || {}, &mut || {
                held = Some(instances_dir_is_held_shared(&home.join("locks")));
            })
            .unwrap();
        assert_eq!(held, Some(true), "{mode:?}");
        drop(lock);
    }
}

/// The op file is made under the hold that the file at the owner's name was compared under.
#[skuld::test]
fn the_op_file_of_a_recreating_try_is_made_under_a_shared_hold_of_instances_dir(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    let mut held = None;
    let probe = locks
        .try_owner_recreating_with(&id, &mut || {}, &mut || {
            held = Some((
                instances_dir_is_held_shared(&home.join("locks")),
                locks.op_path(&id).exists(),
            ));
        })
        .unwrap();
    assert!(matches!(probe, Probe::Free(_)));
    assert_eq!(held, Some((true, false)), "held, and the op file is not made yet");
    assert!(locks.op_path(&id).is_file());
}

/// `instances.dir` is damaged from outside: a lock file that is at its name is not gone.
#[skuld::test]
fn a_lock_file_that_is_there_is_not_gone_when_instances_dir_is_missing(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    let id = InstanceId::random();
    let dir_path = home.join("locks").join("instances.dir");
    let owner = locks.acquire_owner_new(&id).unwrap();

    std::fs::remove_file(&dir_path).unwrap();
    assert!(matches!(locks.try_owner(&id).unwrap(), Probe::Held));
    assert!(dir_path.is_file(), "instances.dir is put back");

    std::fs::remove_file(&dir_path).unwrap();
    assert!(matches!(locks.try_op_exclusive(&id).unwrap(), Probe::Free(_)));
    assert!(dir_path.is_file(), "instances.dir is put back");

    for shared in [false, true] {
        std::fs::remove_file(&dir_path).unwrap();
        let lock = if shared {
            locks.op_shared(&id, "x", &NullProgress)
        } else {
            locks.op_exclusive(&id, "x", &NullProgress)
        };
        drop(lock.expect("the op file is there: the instance is not gone"));
        assert!(dir_path.is_file(), "instances.dir is put back");
    }
    drop(owner);
}

/// A take of `instances.dir` that waited while its file was replaced holds the file at the name, as an instance lock.
#[skuld::test]
fn an_exclusive_take_of_instances_dir_that_waited_while_its_file_was_replaced_holds_the_new_file(
    #[fixture(unicode_home)] home: &Path,
) {
    for change in NameChange::ALL {
        let locks = locks(home);
        drop(locks.acquire_owner_new(&InstanceId::random()).unwrap());
        let dir_path = home.join("locks").join("instances.dir");
        let mut holder = FileLock::open(&dir_path).unwrap();
        assert!(holder.try_lock(Mode::Shared).unwrap());
        let mut holder = Some(holder);
        let excludes = locks
            .excluding_readers(
                &mut || {
                    change.apply(&dir_path);
                    drop(holder.take());
                },
                || !FileLock::open(&dir_path).unwrap().try_lock(Mode::Shared).unwrap(),
            )
            .unwrap();
        assert!(excludes, "{change:?}: the exclusive hold is on the file at the name");
    }
}

#[skuld::test]
fn a_shared_take_of_instances_dir_that_waited_while_its_file_was_replaced_holds_the_new_file(
    #[fixture(unicode_home)] home: &Path,
) {
    for change in NameChange::ALL {
        let locks = locks(home);
        let id = InstanceId::random();
        drop(locks.acquire_owner_new(&id).unwrap());
        let dir_path = home.join("locks").join("instances.dir");
        let mut holder = FileLock::open(&dir_path).unwrap();
        assert!(holder.try_lock(Mode::Exclusive).unwrap());
        let mut holder = Some(holder);
        let mut excludes = None;
        let probe = locks
            .try_owner_with(
                &id,
                &mut || {
                    change.apply(&dir_path);
                    drop(holder.take());
                },
                &mut || excludes = Some(!FileLock::open(&dir_path).unwrap().try_lock(Mode::Exclusive).unwrap()),
            )
            .unwrap();
        assert!(matches!(probe, Probe::Free(_)), "{change:?}");
        assert_eq!(
            excludes,
            Some(true),
            "{change:?}: the shared hold is on the file at the name"
        );
        locks.remove(&id).unwrap();
    }
}

// Build locks =========================================================================================================

/// A build's name: the hex of 16 random bytes, as `Engine::build` draws it.
fn build_hex() -> String {
    hex::encode(crate::random::bytes::<16>())
}

#[skuld::test]
fn acquire_build_new_opens_again_when_the_file_is_unlinked_between_the_open_and_the_look(
    #[fixture(unicode_home)] home: &Path,
) {
    let locks = locks(home);
    let hex = build_hex();
    let path = locks.build_path(&hex);
    let mut calls = 0;
    let lock = locks
        .acquire_build_new_with(
            &hex,
            &mut || {
                calls += 1;
                if calls == 1 {
                    // The sweeper that found the file free and unlinked it.
                    std::fs::remove_file(&path).unwrap();
                }
            },
            &mut || {},
        )
        .unwrap();
    assert_eq!(calls, 2, "the sweeper's progress makes the builder start over");
    assert_eq!(
        lock.is_at_path().unwrap(),
        Comparison::Same,
        "the lock is on the file at the name"
    );
    assert!(matches!(locks.try_build(&hex).unwrap(), Probe::Held));
    drop(lock);
    assert!(matches!(locks.try_build(&hex).unwrap(), Probe::Free(_)));
}

#[skuld::test]
fn a_build_lock_take_on_a_volume_without_posix_unlink_is_not_supported_and_names_the_requirement(
    #[fixture(unicode_home)] home: &Path,
) {
    let strict = without_posix_unlink(home.join("locks"));
    let hex = build_hex();

    let errors = [
        strict
            .acquire_build_new_with(&hex, &mut || {}, &mut || {})
            .map(drop)
            .unwrap_err(),
        strict.remove_build(&hex).unwrap_err(),
    ];
    for error in errors {
        assert_names_the_volume_requirement(error, home);
    }

    // With the directory made on a good volume, the look at an existing lock checks it too.
    let hex = build_hex();
    drop(
        locks(home)
            .acquire_build_new_with(&hex, &mut || {}, &mut || {})
            .unwrap(),
    );
    assert_names_the_volume_requirement(strict.try_build(&hex).map(drop).unwrap_err(), home);
}

#[skuld::test]
fn try_build_creates_nothing_and_reads_a_missing_file_as_gone(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    let hex = build_hex();

    assert!(matches!(locks.try_build(&hex).unwrap(), Probe::Gone), "no directory");
    assert!(!is_there(&home.join("locks")).unwrap(), "a try made the lock directory");

    let held = locks.acquire_build_new_with(&hex, &mut || {}, &mut || {}).unwrap();
    assert!(matches!(locks.try_build(&hex).unwrap(), Probe::Held));
    drop(held);
    assert!(matches!(locks.try_build(&hex).unwrap(), Probe::Free(_)));

    let other = build_hex();
    assert!(
        matches!(locks.try_build(&other).unwrap(), Probe::Gone),
        "no file at the name"
    );
    assert!(!is_there(&locks.build_path(&other)).unwrap(), "a try made the file");
}

#[skuld::test]
fn build_locks_lists_only_build_lock_files_and_ids_ignores_them(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    assert!(locks.build_locks().unwrap().is_empty(), "no directory means no builds");
    assert!(
        !is_there(&home.join("locks")).unwrap(),
        "a listing made the lock directory"
    );

    let (first, second) = (build_hex(), build_hex());
    drop(locks.acquire_build_new_with(&first, &mut || {}, &mut || {}).unwrap());
    drop(locks.acquire_build_new_with(&second, &mut || {}, &mut || {}).unwrap());
    let id = InstanceId::random();
    drop(locks.acquire_owner_new(&id).unwrap());
    let dir = home.join("locks");
    for stray in [
        "build-short.lock".to_owned(),
        format!("build-{}.lock", first.to_uppercase()),
        format!("build-{first}.tmp"),
        format!("{first}.lock"),
        "dead.lock".to_owned(),
    ] {
        std::fs::write(dir.join(stray), "x").unwrap();
    }

    let mut listed = locks.build_locks().unwrap();
    listed.sort();
    let mut expected = vec![first.clone(), second];
    expected.sort();
    assert_eq!(listed, expected);
    assert_eq!(locks.ids().unwrap(), vec![id], "a build lock is no instance's");

    locks.remove_build(&first).unwrap();
    assert!(!is_there(&locks.build_path(&first)).unwrap());
    assert_eq!(locks.build_locks().unwrap().len(), 1);
    locks.remove_build(&first).unwrap();
}

#[skuld::test]
fn unlinking_a_build_lock_waits_for_whoever_is_opening_a_lock_file(#[fixture(unicode_home)] home: &Path) {
    let locks = locks(home);
    let locks = &locks;
    let hex = build_hex();
    drop(locks.acquire_build_new_with(&hex, &mut || {}, &mut || {}).unwrap());
    let (waiting, remover_waits) = std::sync::mpsc::channel();

    std::thread::scope(|scope| {
        let hex = &hex;
        locks
            .reading(home, || {
                let remover = scope.spawn(move || locks.remove_build_with(hex, &mut || waiting.send(()).unwrap()));
                assert!(
                    remover_waits.recv().is_ok(),
                    "the build lock was unlinked while a probe could be opening it"
                );
                assert!(locks.build_path(hex).is_file());
                remover
            })
            .unwrap()
            .unwrap()
            .join()
            .unwrap()
            .unwrap();
    });
    assert!(!is_there(&locks.build_path(&hex)).unwrap());
}
