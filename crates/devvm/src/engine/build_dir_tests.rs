use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::Dirs;
use crate::engine::build_dir::{BuildDir, is_directory, sweep};
use crate::engine::engine_tests::{engine_with, is_there, memory};
use crate::engine::{DEFAULT_BOOT_BOUND, Engine, EngineParts, NoHooks};
use crate::guest::fake::FakeGuests;
use crate::host;
use crate::instance::locks::Probe;
use crate::store::layers::SystemClock;
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::host::{RecordingHost, TestHost};

/// An engine over `Dirs::under(home)` that has no backend: nothing a sweep does needs one.
fn engine_at(home: &Path, progress: Arc<MemoryProgress>) -> Arc<Engine> {
    engine_with(Dirs::under(home), &TestHost, None, Arc::new(FakeGuests), progress)
}

fn hex() -> String {
    hex::encode(crate::random::bytes::<16>())
}

/// A dead build's leftovers: its directory with a snapshot in it, and its lock file, which nothing holds.
fn dead_build(engine: &Engine, dirs: &Dirs) -> (String, PathBuf) {
    let hex = hex();
    drop(
        engine
            .registry()
            .locks()
            .acquire_build_new_with(&hex, &mut || {}, &mut || {})
            .unwrap(),
    );
    let dir = dirs.builds().join(&hex);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("devvm-copy-x.tar"), b"tar").unwrap();
    (hex, dir)
}

fn lock_is_there(engine: &Engine, hex: &str) -> bool {
    host::symlink_metadata_if_present(&engine.registry().locks().build_path(hex))
        .unwrap()
        .is_some()
}

fn name_of(build: &BuildDir) -> String {
    build.path().file_name().unwrap().to_str().unwrap().to_owned()
}

#[skuld::test]
fn a_build_directory_follows_its_lock_and_goes_with_it(#[fixture(unicode_home)] home: &Path) {
    let engine = engine_at(home, memory());
    let dirs = Dirs::under(home);

    let build = BuildDir::create(&engine).unwrap();
    let path = build.path().to_path_buf();
    assert_eq!(path.parent().unwrap(), dirs.builds());
    assert!(path.is_dir());
    let hex = name_of(&build);
    assert!(
        matches!(engine.registry().locks().try_build(&hex).unwrap(), Probe::Held),
        "the lock is held while the build lives"
    );
    std::fs::write(path.join("devvm-copy-x.tar"), b"tar").unwrap();

    drop(build);
    assert!(!is_there(&path), "the directory goes with the guard");
    assert!(!lock_is_there(&engine, &hex), "the lock file goes with the guard");
}

#[skuld::test]
fn a_build_directory_is_removed_when_the_build_unwinds(#[fixture(unicode_home)] home: &Path) {
    let engine = engine_at(home, memory());
    let seen = std::sync::Mutex::new(None);

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let build = BuildDir::create(&engine).unwrap();
        *seen.lock().unwrap() = Some(build.path().to_path_buf());
        panic!("the build panics");
    }));

    assert!(unwound.is_err());
    let path = seen.lock().unwrap().clone().unwrap();
    assert!(!is_there(&path));
    assert!(engine.registry().locks().build_locks().unwrap().is_empty());
}

#[skuld::test]
fn a_sweep_between_the_builders_open_and_lock_cannot_delete_its_directory(#[fixture(unicode_home)] home: &Path) {
    let engine = engine_at(home, memory());

    let mut opens = 0;
    let build = BuildDir::create_with(
        &engine,
        &mut || {
            opens += 1;
            // The sweeper finds the file the builder has opened free, and unlinks it, once.
            if opens == 1 {
                sweep(&engine);
            }
        },
        &mut || {},
    )
    .unwrap();
    assert_eq!(opens, 2, "the builder started over with a file of its own");

    assert!(build.path().is_dir(), "the directory is made after the lock is held");
    assert!(
        matches!(
            engine.registry().locks().try_build(&name_of(&build)).unwrap(),
            Probe::Held
        ),
        "the lock is held on the file at the name"
    );
    // A sweep with the build running leaves it alone.
    sweep(&engine);
    assert!(build.path().is_dir());
}

#[skuld::test]
fn a_build_directory_whose_lock_is_free_is_deleted_by_the_next_reap_with_its_tar(#[fixture(unicode_home)] home: &Path) {
    let engine = engine_at(home, memory());
    let (hex, dir) = dead_build(&engine, &Dirs::under(home));

    engine.reap();

    assert!(!is_there(&dir), "the dead build's directory and its snapshot go");
    assert!(!lock_is_there(&engine, &hex), "and its lock file");
}

#[skuld::test]
fn a_build_directory_whose_lock_is_held_is_kept_by_the_reaper(#[fixture(unicode_home)] home: &Path) {
    let engine = engine_at(home, memory());
    let build = BuildDir::create(&engine).unwrap();
    std::fs::write(build.path().join("devvm-copy-x.tar"), b"tar").unwrap();

    engine.reap();

    assert!(build.path().join("devvm-copy-x.tar").is_file());
    assert_eq!(engine.registry().locks().build_locks().unwrap().len(), 1);
}

#[skuld::test]
fn a_lock_file_with_no_directory_is_deleted_once_its_lock_is_free(#[fixture(unicode_home)] home: &Path) {
    let engine = engine_at(home, memory());
    let locks = engine.registry().locks();
    let hex = hex();
    let held = locks.acquire_build_new_with(&hex, &mut || {}, &mut || {}).unwrap();

    engine.reap();
    assert!(lock_is_there(&engine, &hex), "a held lock stays");

    drop(held);
    engine.reap();
    assert!(!lock_is_there(&engine, &hex));
}

#[skuld::test]
fn a_directory_with_no_lock_file_is_deleted(#[fixture(unicode_home)] home: &Path) {
    let engine = engine_at(home, memory());
    let dirs = Dirs::under(home);
    let dir = dirs.builds().join(hex());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("devvm-copy-x.tar"), b"tar").unwrap();

    engine.reap();

    assert!(!is_there(&dir));
}

#[skuld::test]
fn the_sweep_leaves_other_names_alone_and_creates_nothing(#[fixture(unicode_home)] home: &Path) {
    let progress = memory();
    let engine = engine_at(home, progress.clone());
    let dirs = Dirs::under(home);

    engine.reap();
    assert!(!is_there(&dirs.builds()), "a sweep made the builds directory");
    assert!(!is_there(&dirs.instance_locks()), "a sweep made the lock directory");

    let strays = [
        dirs.builds().join("not-a-build"),
        dirs.builds().join(hex().to_uppercase()),
        dirs.builds().join(&hex()[..30]),
    ];
    for stray in &strays {
        std::fs::create_dir_all(stray).unwrap();
    }
    let file = dirs.builds().join(hex());
    std::fs::write(&file, b"a file, not a build directory").unwrap();

    engine.reap();

    assert!(strays.iter().all(|stray| stray.is_dir()));
    assert!(file.is_file(), "only a directory is a build's");
    assert!(progress.messages().is_empty(), "{:?}", progress.messages());
}

#[skuld::test]
fn a_failed_look_at_a_lock_file_warns_and_deletes_nothing(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let healthy = engine_at(home, memory());
    let (hex, dir) = dead_build(&healthy, &dirs);

    let failing: &'static RecordingHost = Box::leak(Box::new(RecordingHost::new()));
    failing.fail_file_id();
    let progress = memory();
    let engine = Engine::with_parts(EngineParts {
        dirs: dirs.clone(),
        host: failing,
        backend: None,
        guests: Arc::new(FakeGuests),
        clock: Arc::new(SystemClock),
        progress: progress.clone(),
        hooks: Arc::new(NoHooks),
        boot_bound: DEFAULT_BOOT_BOUND,
    });

    engine.reap();

    assert!(dir.is_dir(), "a failed look is not an answer");
    assert!(lock_is_there(&engine, &hex));
    let messages = progress.messages();
    assert!(
        messages.iter().any(|message| message.contains(&hex)),
        "the warning names the build: {messages:?}"
    );
    // The next pass, on a host that can look, finishes the job.
    healthy.reap();
    assert!(!is_there(&dir));
    assert!(!lock_is_there(&healthy, &hex));
}

#[skuld::test]
fn a_directory_with_no_lock_file_is_left_while_another_process_is_deleting(#[fixture(unicode_home)] home: &Path) {
    let engine = engine_at(home, memory());
    let dir = Dirs::under(home).builds().join(hex());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("devvm-copy-x.tar"), b"tar").unwrap();

    let held = engine
        .registry()
        .locks()
        .deleting(|| {
            sweep(&engine);
            is_there(&dir)
        })
        .unwrap();
    assert_eq!(
        held,
        Some(true),
        "a sweep deleted a tree while another deleter held dead.lock"
    );

    sweep(&engine);
    assert!(!is_there(&dir), "the next sweep deletes it");
}

#[skuld::test]
fn a_path_that_is_gone_is_not_a_directory_and_not_a_failure(#[fixture(unicode_home)] home: &Path) {
    assert!(!is_directory(&home.join("gone")).unwrap());
    assert!(is_directory(home).unwrap());
    let file = home.join("file");
    std::fs::write(&file, b"x").unwrap();
    assert!(!is_directory(&file).unwrap());
}
