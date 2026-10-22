use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::exit::Progress;
use crate::recipe::hash::LayerKey;
use crate::store::layers::{
    BuildRequest, Built, ChainLease, Clock, Generation, IdSource, LayerMeta, LayerRef, LayerStore,
};
use crate::store::lock::{FileLock, Mode};
use crate::test_support::fixtures::{FakeClock, MemoryProgress, anna_home};
use crate::{Dirs, ErrorKind};

pub struct Env {
    pub dirs: Dirs,
    pub store: LayerStore,
    pub clock: Arc<FakeClock>,
    pub progress: Arc<MemoryProgress>,
}

/// A store under `home`, with a fake clock at a fixed instant.
pub fn env(home: &Path) -> Env {
    let dirs = Dirs::under(home);
    let clock = Arc::new(FakeClock::new(t0()));
    let progress = Arc::new(MemoryProgress::default());
    let store = LayerStore::open(
        &dirs,
        clock.clone() as Arc<dyn Clock>,
        progress.clone() as Arc<dyn Progress>,
    )
    .unwrap();
    Env {
        dirs,
        store,
        clock,
        progress,
    }
}

pub fn t0() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

pub fn days(n: u64) -> Duration {
    Duration::from_secs(n * 24 * 3600)
}

pub fn key(n: u8) -> LayerKey {
    LayerKey::from_bytes([n; 16])
}

pub fn request(key: LayerKey) -> BuildRequest {
    BuildRequest {
        key,
        source_label: "test source".into(),
    }
}

/// Builds a layer holding one file; no expiry.
pub fn build(env: &Env, key: LayerKey, parent: Option<&ChainLease>) -> ChainLease {
    build_expiring(env, key, parent, None)
}

pub fn build_expiring(
    env: &Env,
    key: LayerKey,
    parent: Option<&ChainLease>,
    expires: Option<SystemTime>,
) -> ChainLease {
    env.store
        .build(&request(key), parent, |staging| {
            std::fs::write(staging.dir.path().join("disk.qcow2"), b"disk").unwrap();
            Ok(Built { expires })
        })
        .unwrap()
}

pub fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[skuld::test]
fn build_then_lookup_hits_and_a_second_build_is_not_run(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let builder = |staging: &crate::store::layers::Staging<'_>| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::fs::write(staging.dir.path().join("disk.qcow2"), b"disk").unwrap();
        Ok(Built { expires: None })
    };
    let first = env.store.build(&request(key(1)), None, builder).unwrap();
    let second = env.store.build(&request(key(1)), None, builder).unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(first.tip().meta().generation, second.tip().meta().generation);
    let hit = env.store.lookup(&key(1)).unwrap().expect("a hit");
    assert_eq!(hit.tip().dir().path(), first.tip().dir().path());
    assert_eq!(
        std::fs::read(hit.tip().dir().path().join("disk.qcow2")).unwrap(),
        b"disk"
    );
    assert!(
        hit.tip().dir().path().starts_with(home),
        "{}",
        hit.tip().dir().path().display()
    );
}

#[skuld::test]
fn lookup_of_unknown_key_is_a_miss(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    assert!(env.store.lookup(&key(9)).unwrap().is_none());
}

#[skuld::test]
fn staging_is_a_sibling_of_the_generation_dir(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let key_dir = env.dirs.layers().join(key(1).to_string());
    let mut staging_depth = 0;
    let lease = env
        .store
        .build(&request(key(1)), None, |staging| {
            assert_eq!(staging.dir.path().parent().unwrap(), key_dir);
            staging_depth = staging.dir.path().components().count();
            Ok(Built { expires: None })
        })
        .unwrap();
    assert_eq!(lease.tip().dir().path().components().count(), staging_depth);
    assert_eq!(lease.tip().dir().path().parent().unwrap(), key_dir);
    assert_eq!(
        lease.tip().dir().path().file_name().unwrap().to_string_lossy(),
        lease.tip().meta().generation.to_string()
    );
}

#[skuld::test]
fn builder_error_leaves_no_staging_and_no_current(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let result = env.store.build(&request(key(1)), None, |staging| {
        std::fs::write(staging.dir.path().join("half"), b"x").unwrap();
        Err(crate::Error::new(ErrorKind::Io, "builder failed"))
    });
    assert_eq!(result.unwrap_err().message(), "builder failed");
    assert_eq!(
        env.progress.messages(),
        Vec::<String>::new(),
        "a clean failure logs nothing"
    );
    assert_eq!(names(&env.dirs.layers().join(key(1).to_string())), Vec::<String>::new());
    assert!(env.store.lookup(&key(1)).unwrap().is_none());
}

#[skuld::test]
fn builder_panic_leaves_no_staging_and_no_current(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = env.store.build(&request(key(1)), None, |staging| {
            std::fs::write(staging.dir.path().join("half"), b"x").unwrap();
            panic!("builder panicked");
        });
    }));
    assert!(outcome.is_err());
    assert_eq!(names(&env.dirs.layers().join(key(1).to_string())), Vec::<String>::new());
    // The build lock died with the unwind: another build goes through.
    build(&env, key(1), None);
}

#[skuld::test]
fn failed_publish_after_the_rename_does_not_report_a_missing_staging_dir(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let key_dir = env.dirs.layers().join(key(1).to_string());
    // A directory named `current` makes the final rename fail, after staging became the generation directory.
    let result = env.store.build(&request(key(1)), None, |_| {
        std::fs::create_dir(key_dir.join("current")).unwrap();
        Ok(Built { expires: None })
    });
    assert_eq!(result.unwrap_err().kind(), ErrorKind::Io);
    assert_eq!(env.progress.messages(), Vec::<String>::new());
}

#[skuld::test]
fn sweep_reports_a_directory_it_cannot_list(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let not_a_dir = home.join("plain-file");
    std::fs::write(&not_a_dir, b"x").unwrap();
    assert_eq!(env.store.sweep_leftovers(&not_a_dir), 0);
    let messages = env.progress.messages();
    assert!(
        messages.iter().any(|m| m.contains("cannot list") && m.contains("plain-file")),
        "{messages:?}"
    );
}

#[skuld::test]
fn build_sweeps_leftover_current_temp_files(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let key_dir = env.dirs.layers().join(key(1).to_string());
    std::fs::create_dir_all(&key_dir).unwrap();
    std::fs::write(key_dir.join("current.tmp-0badc0de"), b"torn").unwrap();
    let lease = build(&env, key(1), None);
    let mut expected = vec!["current".to_owned(), lease.tip().meta().generation.to_string()];
    expected.sort();
    assert_eq!(names(&key_dir), expected);
}

#[skuld::test]
fn child_records_parent_key_and_generation(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let parent = build(&env, key(1), None);
    let child = build(&env, key(2), Some(&parent));
    let parent_ref = LayerRef {
        key: key(1),
        generation: parent.tip().meta().generation,
    };
    assert_eq!(child.tip().meta().parent, Some(parent_ref));
    assert_eq!(child.layers().len(), 2);
    assert_eq!(child.refs(), vec![parent_ref, child.tip().layer_ref()]);
    let looked_up = env.store.lookup(&key(2)).unwrap().unwrap();
    assert_eq!(looked_up.refs(), child.refs());
}

#[skuld::test]
fn child_build_refuses_a_parent_whose_generation_is_gone(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let parent = build(&env, key(1), None);
    drop(build(&env, key(2), Some(&parent)));
    std::fs::remove_dir_all(parent.tip().dir().path()).unwrap();

    assert!(
        env.store.lookup(&key(2)).unwrap().is_none(),
        "a child of a missing parent generation is a miss"
    );
    let error = env
        .store
        .build(&request(key(3)), Some(&parent), |_| Ok(Built { expires: None }))
        .map(|_| ())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Store);
    assert!(
        error.message().contains(&parent.tip().meta().generation.to_string()),
        "{error}"
    );
}

#[skuld::test]
fn meta_round_trips_times_and_ids(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let created = t0() + Duration::new(0, 123_456_789);
    env.clock.set(created);
    let expires = t0() + days(90) + Duration::new(7, 500);
    let lease = build_expiring(&env, key(1), None, Some(expires));
    let meta: &LayerMeta = lease.tip().meta();
    assert_eq!(meta.key, key(1));
    assert_eq!(meta.created, created);
    assert_eq!(meta.expires, Some(expires));
    assert_eq!(meta.parent, None);

    let reread = env.store.lookup(&key(1)).unwrap().unwrap();
    assert_eq!(reread.tip().meta().generation, meta.generation);
    assert_eq!(reread.tip().meta().created, created);
    assert_eq!(reread.tip().meta().expires, Some(expires));

    let text = std::fs::read_to_string(lease.tip().dir().path().join("meta.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["schema"], 1);
    assert_eq!(json["key"], key(1).to_string());
    assert_eq!(json["generation"], meta.generation.to_string());
    assert!(json["expires"].as_str().unwrap().ends_with('Z'), "{text}");
    assert_eq!(
        meta.generation.to_string().parse::<Generation>().unwrap(),
        meta.generation
    );
}

/// Hands out fixed generations, checking each time that the key's build lock is held by someone else.
struct ProbingIds {
    lock_path: std::path::PathBuf,
    next: std::sync::atomic::AtomicU8,
}

impl IdSource for ProbingIds {
    fn generation(&self) -> Generation {
        let mut probe = FileLock::open(&self.lock_path).unwrap();
        assert!(
            !probe.try_lock(Mode::Exclusive).unwrap(),
            "generation allocated without the build lock"
        );
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Generation::from_bytes([n; 16])
    }
}

#[skuld::test]
fn generation_ids_are_allocated_only_under_the_build_lock(#[fixture(anna_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let ids = Arc::new(ProbingIds {
        lock_path: dirs.layers().join(".locks").join(format!("{}.build", key(1))),
        next: std::sync::atomic::AtomicU8::new(0xa0),
    });
    let clock = Arc::new(FakeClock::new(t0()));
    let store = LayerStore::open_with_ids(&dirs, clock, Arc::new(crate::exit::NullProgress), ids).unwrap();
    let lease = store
        .build(&request(key(1)), None, |staging| {
            assert_eq!(staging.generation, Generation::from_bytes([0xa0; 16]));
            Ok(Built { expires: None })
        })
        .unwrap();
    assert_eq!(lease.tip().meta().generation, Generation::from_bytes([0xa0; 16]));
}

#[skuld::test]
fn built_from_meta_takes_only_the_expiry() {
    let meta = LayerMeta {
        key: key(7),
        generation: Generation::from_bytes([7; 16]),
        parent: Some(LayerRef {
            key: key(6),
            generation: Generation::from_bytes([6; 16]),
        }),
        created: t0(),
        expires: Some(t0() + days(30)),
    };
    assert_eq!(Built::from_meta(&meta).expires, Some(t0() + days(30)));
}

#[skuld::test]
fn store_types_are_send_and_sync() {
    fn assert_bounds<T: Send + Sync>() {}
    assert_bounds::<LayerStore>();
    assert_bounds::<ChainLease>();
    assert_bounds::<crate::store::layers::LayerLease>();
}

// Generations and expiry ==============================================================================================

use crate::store::layers::EXPIRY_MARGIN;

fn current_of(env: &Env, key: LayerKey) -> String {
    std::fs::read_to_string(env.dirs.layers().join(key.to_string()).join("current")).unwrap()
}

#[skuld::test]
fn near_expiry_base_and_child_both_miss_and_rebuild_as_new_generations(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let base = build_expiring(&env, key(1), None, Some(t0() + days(90)));
    let child = build_expiring(&env, key(2), Some(&base), Some(t0() + days(120)));
    let (old_base, old_child) = (base.tip().meta().generation, child.tip().meta().generation);
    let (old_base_dir, old_child_dir) = (base.tip().dir().path().to_owned(), child.tip().dir().path().to_owned());

    env.clock.set(t0() + days(80) + Duration::from_secs(1));
    assert!(env.store.lookup(&key(1)).unwrap().is_none());
    assert!(env.store.lookup(&key(2)).unwrap().is_none());

    let now = t0() + days(80) + Duration::from_secs(1);
    let new_base = build_expiring(&env, key(1), None, Some(now + days(90)));
    let new_child = build_expiring(&env, key(2), Some(&new_base), Some(now + days(120)));
    assert_ne!(new_base.tip().meta().generation, old_base);
    assert_ne!(new_child.tip().meta().generation, old_child);
    assert_eq!(current_of(&env, key(1)), new_base.tip().meta().generation.to_string());
    assert_eq!(current_of(&env, key(2)), new_child.tip().meta().generation.to_string());
    assert_eq!(
        new_child.tip().meta().parent.unwrap().generation,
        new_base.tip().meta().generation
    );

    // The old generations, which instances may still be using, keep their files.
    assert_eq!(std::fs::read(old_base_dir.join("disk.qcow2")).unwrap(), b"disk");
    assert_eq!(std::fs::read(old_child_dir.join("disk.qcow2")).unwrap(), b"disk");
}

#[skuld::test]
fn lookup_boundary_is_exactly_ten_days(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let expires = t0() + days(100);
    drop(build_expiring(&env, key(1), None, Some(expires)));
    assert_eq!(EXPIRY_MARGIN, days(10));
    env.clock.set(expires - EXPIRY_MARGIN - Duration::from_secs(1));
    assert!(env.store.lookup(&key(1)).unwrap().is_some());
    env.clock.set(expires - EXPIRY_MARGIN);
    assert!(env.store.lookup(&key(1)).unwrap().is_none());
    env.clock.set(expires + days(1));
    assert!(env.store.lookup(&key(1)).unwrap().is_none());
}

#[skuld::test]
fn child_expiry_is_the_minimum_of_own_and_parent(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let parent = build_expiring(&env, key(1), None, Some(t0() + days(90)));
    let later = build_expiring(&env, key(2), Some(&parent), Some(t0() + days(200)));
    assert_eq!(later.tip().meta().expires, Some(t0() + days(90)));
    let sooner = build_expiring(&env, key(3), Some(&parent), Some(t0() + days(30)));
    assert_eq!(sooner.tip().meta().expires, Some(t0() + days(30)));
    let unlimited = build_expiring(&env, key(4), Some(&parent), None);
    assert_eq!(unlimited.tip().meta().expires, Some(t0() + days(90)));
}

#[skuld::test]
fn born_expired_build_fails_with_expired_base_and_publishes_nothing(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let expires = t0() + days(3);
    let error = env
        .store
        .build(&request(key(1)), None, |_| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Built { expires: Some(expires) })
        })
        .map(|_| ())
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ExpiredBase);
    assert!(error.message().contains("2027-01-18"), "{error}");
    assert!(error.message().contains("test source"), "{error}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1, "it never loops");
    assert_eq!(names(&env.dirs.layers().join(key(1).to_string())), Vec::<String>::new());
}

#[skuld::test]
fn failed_expiry_triggered_rebuild_is_an_error_not_the_old_generation(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let old = build_expiring(&env, key(1), None, Some(t0() + days(90)));
    let old_generation = old.tip().meta().generation;
    env.clock.set(t0() + days(85));
    let result = env.store.build(&request(key(1)), None, |_| {
        Err(crate::Error::new(ErrorKind::Io, "rebuild failed"))
    });
    assert_eq!(result.map(|_| ()).unwrap_err().message(), "rebuild failed");
    assert!(env.store.lookup(&key(1)).unwrap().is_none());
    assert_eq!(current_of(&env, key(1)), old_generation.to_string());
    assert_eq!(
        std::fs::read(old.tip().dir().path().join("disk.qcow2")).unwrap(),
        b"disk"
    );
}

/// Reads one hour later every time it is asked.
struct StepClock(std::sync::Mutex<SystemTime>);

impl Clock for StepClock {
    fn now(&self) -> SystemTime {
        let mut now = self.0.lock().unwrap();
        let read = *now;
        *now += Duration::from_secs(3600);
        read
    }
}

#[skuld::test]
fn a_layer_expiring_between_the_stamp_and_the_read_back_is_still_returned(#[fixture(anna_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let store = LayerStore::open(
        &dirs,
        Arc::new(StepClock(std::sync::Mutex::new(t0()))),
        Arc::new(crate::exit::NullProgress),
    )
    .unwrap();
    // The stamp is the third clock read (t0 + 2h); the read-back after publishing would be a fourth (t0 + 3h).
    let expires = t0() + EXPIRY_MARGIN + Duration::from_secs(2 * 3600 + 1800);
    let lease = store
        .build(&request(key(1)), None, |_| Ok(Built { expires: Some(expires) }))
        .unwrap();
    assert_eq!(lease.tip().meta().expires, Some(expires));
}

struct ChannelProgress(std::sync::Mutex<std::sync::mpsc::Sender<String>>);

impl Progress for ChannelProgress {
    fn message(&self, text: &str) {
        self.0.lock().unwrap().send(text.to_owned()).unwrap();
    }
}

#[skuld::test]
fn waiter_second_lookup_uses_a_fresh_clock_read(#[fixture(anna_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let clock = Arc::new(FakeClock::new(t0()));
    let (messages, waiting) = std::sync::mpsc::channel();
    let store = LayerStore::open(
        &dirs,
        clock.clone() as Arc<dyn Clock>,
        Arc::new(ChannelProgress(std::sync::Mutex::new(messages))),
    )
    .unwrap();
    let (building_tx, building_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let waiter_builds = std::sync::atomic::AtomicUsize::new(0);

    let (a_generation, b_generation) = std::thread::scope(|scope| {
        let store = &store;
        let a = scope.spawn(move || {
            let lease = store
                .build(&request(key(1)), None, |_| {
                    building_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(Built {
                        expires: Some(t0() + days(90)),
                    })
                })
                .unwrap();
            lease.tip().meta().generation
        });
        building_rx.recv().unwrap();
        let (clock, waiter_builds) = (&clock, &waiter_builds);
        let b = scope.spawn(move || {
            // B's first clock read is at T0; its second, once it holds the build lock, is 85 days later.
            clock.script_this_thread(t0() + days(85));
            let lease = store
                .build(&request(key(1)), None, |_| {
                    waiter_builds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(Built {
                        expires: Some(t0() + days(85) + days(90)),
                    })
                })
                .unwrap();
            lease.tip().meta().generation
        });
        let message = waiting.recv().unwrap();
        assert!(
            message.starts_with("waiting for another devvm process building layer"),
            "{message}"
        );
        release_tx.send(()).unwrap();
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_eq!(
        waiter_builds.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a clock read before the wait would have hit A's layer, which is within the margin by now"
    );
    assert_ne!(a_generation, b_generation);
}

#[skuld::test]
fn waiter_whose_layer_was_built_meanwhile_does_not_build_again(#[fixture(anna_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let (messages, waiting) = std::sync::mpsc::channel();
    let store = LayerStore::open(
        &dirs,
        Arc::new(FakeClock::new(t0())),
        Arc::new(ChannelProgress(std::sync::Mutex::new(messages))),
    )
    .unwrap();
    let (building_tx, building_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let waiter_builds = std::sync::atomic::AtomicUsize::new(0);

    let (a_generation, b_generation) = std::thread::scope(|scope| {
        let store = &store;
        let a = scope.spawn(move || {
            let lease = store
                .build(&request(key(1)), None, |_| {
                    building_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(Built { expires: None })
                })
                .unwrap();
            lease.tip().meta().generation
        });
        building_rx.recv().unwrap();
        let waiter_builds = &waiter_builds;
        let b = scope.spawn(move || {
            let lease = store
                .build(&request(key(1)), None, |_| {
                    waiter_builds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(Built { expires: None })
                })
                .unwrap();
            lease.tip().meta().generation
        });
        let message = waiting.recv().unwrap();
        assert!(
            message.starts_with("waiting for another devvm process building layer"),
            "{message}"
        );
        release_tx.send(()).unwrap();
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_eq!(waiter_builds.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(a_generation, b_generation);
}
