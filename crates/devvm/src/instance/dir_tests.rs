use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::instance::registry::registry_tests::{create, fixture};
use crate::store::layers::Reachability;
use crate::test_support::fixtures::anna_home;

#[skuld::test]
fn destroy_never_fails_because_other_devvm_readers_run(#[fixture(anna_home)] home: &Path) {
    const ROUNDS: usize = 200;
    let fx = fixture(home);
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        // Each reader is a thing every devvm command does: the reaper pass, `ls`, and the layer collection's scan.
        let readers = [
            scope.spawn(|| {
                while !done.load(Ordering::Acquire) {
                    fx.registry.reap();
                }
            }),
            scope.spawn(|| {
                while !done.load(Ordering::Acquire) {
                    for dir in fx.registry.list().unwrap() {
                        let _ = dir.record();
                    }
                }
            }),
            scope.spawn(|| {
                while !done.load(Ordering::Acquire) {
                    fx.registry.chains().unwrap();
                }
            }),
        ];
        for round in 0..ROUNDS {
            let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
            let op = dir.op_exclusive().unwrap();
            if let Err(e) = dir.destroy(op) {
                done.store(true, Ordering::Release);
                panic!("round {round}: {}", e.message());
            }
        }
        done.store(true, Ordering::Release);
        for reader in readers {
            reader.join().unwrap();
        }
    });
}

#[skuld::test]
fn destroy_waits_for_a_reader_that_has_a_file_open(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let op = dir.op_exclusive().unwrap();
    let (contended, contention) = std::sync::mpsc::channel();

    std::thread::scope(|scope| {
        let destroyer = fx
            .registry
            .locks()
            .reading(dir.path(), || {
                // On Windows this handle alone makes a rename of the directory fail.
                let _open = std::fs::File::open(dir.path().join("record.json")).unwrap();
                let destroyer = scope.spawn(|| dir.destroy_with(op, &mut || contended.send(()).unwrap()));
                contention.recv().unwrap();
                destroyer
            })
            .unwrap()
            .unwrap();
        destroyer.join().unwrap().unwrap();
    });
    assert!(!fx.env.dirs.instances().join("foo").exists());
}

#[skuld::test]
fn two_deleters_of_one_dead_tree_do_not_warn(#[fixture(anna_home)] home: &Path) {
    const ROUNDS: usize = 40;
    let fx = fixture(home);
    let done = AtomicBool::new(false);
    let warnings = std::thread::scope(|scope| {
        let reaper = scope.spawn(|| {
            let mut warnings = Vec::new();
            while !done.load(Ordering::Acquire) {
                warnings.extend(fx.registry.reap().warnings);
            }
            warnings
        });
        for _ in 0..ROUNDS {
            let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
            let files = dir.path().join("files");
            std::fs::create_dir(&files).unwrap();
            for i in 0..300 {
                std::fs::write(files.join(i.to_string()), "x").unwrap();
            }
            let op = dir.op_exclusive().unwrap();
            dir.destroy(op).unwrap();
        }
        done.store(true, Ordering::Release);
        reaper.join().unwrap()
    });
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(fx.progress.messages().is_empty(), "{:?}", fx.progress.messages());
}

#[skuld::test]
fn destroy_deletes_its_own_tree_even_while_another_deletion_runs(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let dead = fx.env.dirs.instances().join(format!(".dead-{}", dir.id()));
    let op = dir.op_exclusive().unwrap();
    let locks = fx.registry.locks();
    let (held, other_deletion) = std::sync::mpsc::channel();
    let (contended, waiting) = std::sync::mpsc::channel();

    std::thread::scope(|scope| {
        let other = scope.spawn(move || {
            locks
                .deleting(|| {
                    held.send(()).unwrap();
                    // Ends when `destroy` waits, or, if it does not wait, when the sender is dropped.
                    let _ = waiting.recv();
                })
                .unwrap()
        });
        other_deletion.recv().unwrap();
        dir.destroy_with_waits(op, &mut || {}, &mut move || contended.send(()).unwrap())
            .unwrap();
        assert!(!dead.exists(), "rm left its tree for the next command");
        other.join().unwrap();
    });
    assert!(fx.progress.messages().is_empty(), "{:?}", fx.progress.messages());
}

/// Runs `destroy` of `dir` while another deletion holds the tree lock, and calls `sabotage` in that deletion once
/// `destroy` has renamed the directory and waits for it: the state of the renamed tree and of the lock files can then
/// be spoiled between the rename and the cleanup.
pub(crate) fn destroy_while_a_deletion_runs<T: Send>(
    fx: &crate::instance::registry::registry_tests::Fixture,
    dir: &crate::instance::dir::InstanceDir,
    sabotage: impl FnOnce() -> T + Send,
) -> (crate::Result<()>, T) {
    let op = dir.op_exclusive().unwrap();
    let locks = fx.registry.locks();
    let (held, other_deletion) = std::sync::mpsc::channel();
    let (contended, waiting) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let other = scope.spawn(move || {
            locks
                .deleting(|| {
                    held.send(()).unwrap();
                    waiting.recv().unwrap();
                    sabotage()
                })
                .unwrap()
                .unwrap()
        });
        other_deletion.recv().unwrap();
        let destroyed = dir.destroy_with_waits(op, &mut || {}, &mut move || contended.send(()).unwrap());
        (destroyed, other.join().unwrap())
    })
}

#[skuld::test]
fn destroy_warns_but_succeeds_when_the_lock_files_cannot_be_deleted(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let op_file = fx.registry.locks().op_path(dir.id());

    let (destroyed, ()) = destroy_while_a_deletion_runs(&fx, &dir, || {
        // The lock file is replaced by a directory, which no unlink of a file can remove.
        std::fs::remove_file(&op_file).unwrap();
        std::fs::create_dir(&op_file).unwrap();
        std::fs::write(op_file.join("x"), "x").unwrap();
    });

    destroyed.expect("the instance is removed, so rm succeeds");
    assert!(!fx.env.dirs.instances().join("foo").exists());
    let messages = fx.progress.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0].contains("foo") && messages[0].contains("lock files could not be deleted"),
        "{messages:?}"
    );
}

#[skuld::test]
fn handing_over_an_instance_with_a_corrupt_record_reports_the_record_and_writes_nothing(
    #[fixture(anna_home)] home: &Path,
) {
    let fx = fixture(home);
    let owned = create(&fx, Some("foo"));
    let instance = fx.env.dirs.instances().join("foo");
    std::fs::write(instance.join("record.json"), "{ torn").unwrap();
    let marker_before = std::fs::read(instance.join("owner.json")).unwrap();

    let error = owned.make_ownerless(&mut || Ok(())).err().expect("the record is torn");

    assert_eq!(error.kind(), crate::ErrorKind::Store);
    assert!(
        error.message().contains("unreadable") && error.message().contains("foo"),
        "{}",
        error.message()
    );
    assert_eq!(std::fs::read(instance.join("owner.json")).unwrap(), marker_before);
}
