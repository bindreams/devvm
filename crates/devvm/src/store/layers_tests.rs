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
        let probe = FileLock::open(&self.lock_path).unwrap();
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
