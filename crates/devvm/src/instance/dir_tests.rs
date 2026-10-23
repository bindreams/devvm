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
