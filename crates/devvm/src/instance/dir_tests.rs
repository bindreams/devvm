use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::backend::MachineRecord;
use crate::contain::refused_record;
use crate::instance::dir::InstanceDir;
use crate::instance::locks::Probe;
use crate::instance::record::{HOLDER_FILE, Holder, read_holder, write_holder};
use crate::instance::registry::registry_tests::{Fixture, create, fixture};
use crate::progress::{Event, Level, Progress};
use crate::store::layers::Reachability;
use crate::test_support::fixtures::{ChannelProgress, MemoryProgress, unicode_home};
use crate::test_support::helper;
use crate::test_support::qemu::{Sock, backend_in, socket_run};
use crate::test_support::stub_backend::{Attach, StubBackend};

#[skuld::test]
fn destroy_never_fails_because_other_devvm_readers_run(#[fixture(unicode_home)] home: &Path) {
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
fn destroy_waits_for_a_reader_that_has_a_file_open(#[fixture(unicode_home)] home: &Path) {
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
fn two_deleters_of_one_dead_tree_do_not_warn(#[fixture(unicode_home)] home: &Path) {
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
fn destroy_deletes_its_own_tree_even_while_another_deletion_runs(#[fixture(unicode_home)] home: &Path) {
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
fn destroy_is_done_when_the_deletion_it_waited_for_took_its_tree(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let dead = fx.env.dirs.instances().join(format!(".dead-{}", dir.id()));

    let (destroyed, ()) = destroy_while_a_deletion_runs(&fx, &dir, || std::fs::remove_dir_all(&dead).unwrap());

    destroyed.expect("the tree is gone, as rm promises");
    assert!(fx.progress.messages().is_empty(), "{:?}", fx.progress.messages());
}

#[skuld::test]
fn destroy_deletes_a_file_that_took_the_name_of_its_tree(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let dead = fx.env.dirs.instances().join(format!(".dead-{}", dir.id()));

    let (destroyed, ()) = destroy_while_a_deletion_runs(&fx, &dir, || {
        std::fs::remove_dir_all(&dead).unwrap();
        std::fs::write(&dead, "x").unwrap();
    });

    destroyed.expect("rm succeeds");
    assert!(
        std::fs::symlink_metadata(&dead).is_err(),
        "rm left a file at the name of its tree"
    );
    assert!(fx.progress.messages().is_empty(), "{:?}", fx.progress.messages());
}

#[skuld::test]
fn destroy_warns_but_succeeds_when_the_lock_files_cannot_be_deleted(#[fixture(unicode_home)] home: &Path) {
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
    let messages = fx.progress.warnings();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0].contains("foo") && messages[0].contains("lock files could not be deleted"),
        "{messages:?}"
    );
    assert!(
        matches!(
            fx.progress.events().as_slice(),
            [Event::Note { level: Level::Warn, .. }]
        ),
        "a failed removal is a warning: {:?}",
        fx.progress.events()
    );
}

#[skuld::test]
fn handing_over_an_instance_with_a_corrupt_record_reports_the_record_and_writes_nothing(
    #[fixture(unicode_home)] home: &Path,
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

/// `rm` and `stop` need the machine found, so a record that cannot be read stops them.
#[skuld::test]
fn finding_a_machine_whose_process_record_cannot_be_read_says_how_to_recover(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let sock = Sock::new();
    let cosca::identity::Resolved::Found(id) = cosca::identity::ProcessId::of(std::process::id()) else {
        panic!("the test process cannot be identified");
    };
    let mut process = id.to_record().unwrap();
    process.version = cosca::identity::RECORD_VERSION + 1;
    let machine = MachineRecord {
        process,
        qmp: sock.endpoint.path().display().to_string(),
        ssh: Vec::new(),
        serial_log: sock.endpoint.path().with_file_name("serial.log"),
        leftovers: Vec::new(),
        claim: Some(sock.claim.clone()),
        no_reboot: false,
    };
    let backend = backend_in(&socket_run(), std::sync::Arc::<MemoryProgress>::default());

    let error = dir
        .find_machine(&*backend, &machine)
        .err()
        .expect("a record this build cannot read is not read as not running");

    assert_eq!(error.kind(), crate::ErrorKind::ProcessRecord, "{error}");
    let text = error.message().to_owned();
    let record = dir.path().join("record.json").display().to_string();
    let cause = "the VM's process record has a format this devvm does not read (";
    let step = format!("; stop the VM by hand if it still runs, then clear the `machine` field in {record}");
    assert!(text.starts_with(cause), "the cause leads, on its own: {text}");
    let cosca = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<cosca::error::Error>())
        .expect("the cosca error stays the source");
    let report = cosca::error::Report::new(cosca).to_string();
    assert!(
        text.ends_with(&format!(" ({report}){step}")),
        "cosca's words, then the step added after them: {text}"
    );
    assert_eq!(text.matches(&report).count(), 1, "cosca's words once: {text}");
}

/// Only a process record that cannot be read has a way out by hand: any other failure of the backend passes through.
#[skuld::test]
fn a_failure_of_the_backend_that_is_not_an_unreadable_record_passes_through_find(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let machine = MachineRecord {
        process: cosca::identity::ProcessId::current().to_record().unwrap(),
        qmp: String::new(),
        ssh: Vec::new(),
        serial_log: PathBuf::new(),
        leftovers: Vec::new(),
        claim: None,
        no_reboot: false,
    };

    let backend = StubBackend::new(Attach::Fails(crate::ErrorKind::Io));
    let error = dir.find_machine(&*backend, &machine).err().unwrap();
    assert_eq!(error.message(), "the stub backend cannot look");

    let refused = refused_record(7, crate::contain::contain_tests::scope_refusal())
        .err()
        .expect("liveness cannot be established");
    let backend = StubBackend::new(Attach::FailsWith(refused.clone()));
    let error = dir.find_machine(&*backend, &machine).err().unwrap();
    assert_eq!(error.kind(), crate::ErrorKind::Io, "{error}");
    assert_eq!(error.message(), refused.message(), "no step is added");
}

// The holder of the exclusive op lock =================================================================================

/// A handle on the instance `dir` that reports to `progress`: another devvm command on the same instance.
fn another_command(fx: &Fixture, dir: &InstanceDir, progress: Arc<dyn Progress>) -> InstanceDir {
    InstanceDir::new(
        *dir.id(),
        dir.name().clone(),
        dir.path().to_owned(),
        fx.registry.locks().clone(),
        progress,
    )
}

/// What this test binary calls itself in `waiting for <command> (PID <pid>) on instance <name>`.
fn this_command() -> String {
    let exe = std::env::current_exe().unwrap();
    exe.file_stem().unwrap().to_string_lossy().into_owned()
}

fn named_holder(message: &str, pid: u32, instance: &str) {
    let command = this_command();
    assert!(message.starts_with(&format!("waiting for {command}")), "{message}");
    assert!(
        message.ends_with(&format!(" (PID {pid}) on instance {instance}")),
        "{message}"
    );
}

#[skuld::test]
fn a_wait_behind_the_exclusive_holder_names_its_command_and_pid(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let holder = dir.op_exclusive().unwrap();
    let (progress, messages) = ChannelProgress::new();
    let waiter = another_command(&fx, &dir, progress);

    std::thread::scope(|scope| {
        let waiting = scope.spawn(|| waiter.op_exclusive().map(drop));
        named_holder(&messages.recv().unwrap(), std::process::id(), "foo");
        drop(holder);
        waiting.join().unwrap().unwrap();
    });
}

#[skuld::test]
fn a_shared_wait_behind_the_exclusive_holder_names_it_too(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let holder = dir.op_exclusive().unwrap();
    let (progress, messages) = ChannelProgress::new();
    let waiter = another_command(&fx, &dir, progress);

    std::thread::scope(|scope| {
        let waiting = scope.spawn(|| waiter.op_shared().map(drop));
        named_holder(&messages.recv().unwrap(), std::process::id(), "foo");
        drop(holder);
        waiting.join().unwrap().unwrap();
    });
}

#[skuld::test]
fn a_wait_behind_a_record_of_a_dead_process_names_no_one(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let holder = dir.op_exclusive().unwrap();
    // The identity of this process with another start token: it names no process that exists.
    let mut record = read_holder(dir.path()).unwrap().expect("the holder is recorded");
    record.process.token = record.process.token.wrapping_add(1);
    write_holder(dir.path(), &record).unwrap();
    let (progress, messages) = ChannelProgress::new();
    let waiter = another_command(&fx, &dir, progress);

    std::thread::scope(|scope| {
        let waiting = scope.spawn(|| waiter.op_exclusive().map(drop));
        assert_eq!(
            messages.recv().unwrap(),
            "waiting for another devvm command on instance foo"
        );
        drop(holder);
        waiting.join().unwrap().unwrap();
    });
}

#[skuld::test]
fn a_wait_behind_a_holder_without_a_record_keeps_the_text(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let holder = dir.op_exclusive().unwrap();
    std::fs::remove_file(dir.path().join(HOLDER_FILE)).unwrap();
    let (progress, messages) = ChannelProgress::new();
    let waiter = another_command(&fx, &dir, progress);

    std::thread::scope(|scope| {
        let waiting = scope.spawn(|| waiter.op_exclusive().map(drop));
        assert_eq!(
            messages.recv().unwrap(),
            "waiting for another devvm command on instance foo"
        );
        drop(holder);
        waiting.join().unwrap().unwrap();
    });
}

#[skuld::test]
fn releasing_the_op_lock_removes_the_holder_record(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();

    let holder = dir.op_exclusive().unwrap();
    let record = read_holder(dir.path()).unwrap().expect("the holder is recorded");
    assert_eq!(record.process.pid, std::process::id());
    drop(holder);

    assert!(read_holder(dir.path()).unwrap().is_none());
}

#[skuld::test]
fn a_failed_holder_write_warns_and_the_lock_is_still_taken(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    // A directory at the name of the record: the write cannot replace it.
    std::fs::create_dir(dir.path().join(HOLDER_FILE)).unwrap();

    let holder = dir.op_exclusive().expect("the command goes on without its record");

    assert!(matches!(
        fx.registry.locks().try_op_exclusive(dir.id()).unwrap(),
        Probe::Held
    ));
    let warnings = fx.progress.warnings();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("foo") && warnings[0].contains(HOLDER_FILE),
        "{warnings:?}"
    );
    drop(holder);
}

#[skuld::test]
fn destroying_an_instance_under_its_op_lock_does_not_fail_when_the_record_goes_with_it(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let op = dir.op_exclusive().unwrap();
    assert!(read_holder(dir.path()).unwrap().is_some());

    dir.destroy(op).unwrap();

    assert!(!fx.env.dirs.instances().join("foo").exists());
    assert!(fx.progress.messages().is_empty(), "{:?}", fx.progress.messages());
}

#[skuld::test]
fn a_holder_of_a_removed_instance_leaves_the_record_of_a_new_instance_of_the_name(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = fixture(home);
    let old = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let holder = old.op_exclusive().unwrap();
    // `rm` by another command, then a new instance with the same name and a holder of its own.
    std::fs::remove_dir_all(old.path()).unwrap();
    let new = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let new_holder = new.op_exclusive().unwrap();
    assert!(read_holder(new.path()).unwrap().is_some());

    drop(holder);

    assert!(
        read_holder(new.path()).unwrap().is_some(),
        "the old holder removed the new one's record"
    );
    drop(new_holder);
}

/// Spawns a process: it runs in CI only. A helper process holds the op lock without a record of its own, and the
/// record names it, as a devvm command in another process would.
#[skuld::test]
fn a_wait_behind_a_holder_in_another_process_names_that_pid(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let op_file = fx.registry.locks().op_path(dir.id());
    let mut child = helper::command("hold-lock", &[op_file.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "held");
    let cosca::identity::Resolved::Found(id) = cosca::identity::ProcessId::of(child.id()) else {
        panic!("the helper cannot be identified");
    };
    let holder = Holder {
        process: id.to_record().unwrap(),
        command: "devvm up".to_owned(),
    };
    write_holder(dir.path(), &holder).unwrap();
    let (progress, messages) = ChannelProgress::new();
    let waiter = another_command(&fx, &dir, progress);

    std::thread::scope(|scope| {
        let waiting = scope.spawn(|| waiter.op_exclusive().map(drop));
        assert_eq!(
            messages.recv().unwrap(),
            format!("waiting for devvm up (PID {}) on instance foo", child.id())
        );
        child.kill().unwrap();
        child.wait().unwrap();
        // The OS releases a dead holder's lock asynchronously; the waiter's blocking take is the event awaited.
        waiting.join().unwrap().unwrap();
    });
}
