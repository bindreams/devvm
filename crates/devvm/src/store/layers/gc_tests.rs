use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::mpsc;

use crate::exit::{NullProgress, Progress};
use crate::store::layers::{Clock, GcReport, Generation, LayerRef, LayerStore, Reachability, SystemClock};
use crate::store::lock::Mode;
use crate::test_support::fixtures::{FakeClock, anna_home};
use crate::test_support::helper;
use crate::{Dirs, Error, ErrorKind, Result};

use super::cross_tests::start as start_builder;
use super::layers_tests::{Env, build_expiring, days, env, key, names, t0};

/// Helper process: leases generation `args[2]` of key `args[1]` in the store rooted at `args[0]`, prints `held`, and
/// blocks until stdin closes.
pub fn lease_holder(args: &[String]) {
    let dirs = Dirs::under(Path::new(&args[0]));
    let store = LayerStore::open(&dirs, Arc::new(SystemClock), Arc::new(NullProgress)).unwrap();
    let layer = LayerRef {
        key: args[1].parse().unwrap(),
        generation: args[2].parse().unwrap(),
    };
    let guard = store.begin_record().unwrap();
    let lease = store.lease_chain(&layer, &guard).unwrap();
    drop(guard);
    println!("held");
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
    drop(lease);
}

struct Fixed(Vec<Vec<LayerRef>>);

impl Reachability for Fixed {
    fn chains(&self) -> Result<Vec<Vec<LayerRef>>> {
        Ok(self.0.clone())
    }
}

struct Failing;

impl Reachability for Failing {
    fn chains(&self) -> Result<Vec<Vec<LayerRef>>> {
        Err(Error::new(ErrorKind::Io, "cannot list the instance records"))
    }
}

/// Builds key `n` three times, each time after the previous generation has aged into the expiry margin. Returns the
/// generations oldest first; the clock ends well past the third build's start.
fn three_generations(env: &Env, n: u8) -> [LayerRef; 3] {
    let mut refs = Vec::new();
    for round in 0..3 {
        env.clock.set(t0() + days(round * 80 + 1));
        let lease = build_expiring(env, key(n), None, Some(t0() + days(round * 80 + 1) + days(90)));
        refs.push(lease.tip().layer_ref());
    }
    refs.try_into().unwrap()
}

fn exists(env: &Env, layer: &LayerRef) -> bool {
    env.store.generation_dir(layer).join("disk.qcow2").is_file()
}

#[skuld::test]
fn gc_deletes_only_unreachable_generations(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let [oldest, middle, newest] = three_generations(&env, 1);
    let report = env.store.gc(&Fixed(vec![vec![middle]])).unwrap();
    assert_eq!(
        report,
        GcReport {
            deleted: vec![oldest],
            held: vec![],
            staging_removed: 0
        }
    );
    assert!(!exists(&env, &oldest));
    assert!(exists(&env, &middle) && exists(&env, &newest));
    assert!(
        !env.dirs
            .layers()
            .join(".locks")
            .join(format!("{}-{}.gen", oldest.key, oldest.generation))
            .exists()
    );
}

#[skuld::test]
fn gc_never_deletes_a_generation_held_by_a_live_image_handle(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let [oldest, _middle, _newest] = three_generations(&env, 1);
    let guard = env.store.begin_record().unwrap();
    let image = env.store.lease_chain(&oldest, &guard).unwrap();
    drop(guard);

    let report = env.store.gc(&Fixed(vec![])).unwrap();
    assert!(report.held.contains(&oldest), "{report:?}");
    assert!(!report.deleted.contains(&oldest));
    assert!(exists(&env, &oldest));

    drop(image);
    let report = env.store.gc(&Fixed(vec![])).unwrap();
    assert!(report.deleted.contains(&oldest), "{report:?}");
    assert!(!exists(&env, &oldest));
}

#[skuld::test]
fn gc_respects_a_lease_held_by_another_process(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let [oldest, _middle, _newest] = three_generations(&env, 1);
    let mut child = helper::command(
        "lease-holder",
        &[
            home.to_str().unwrap(),
            &oldest.key.to_string(),
            &oldest.generation.to_string(),
        ],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()
    .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "held");

    let report = env.store.gc(&Fixed(vec![])).unwrap();
    assert!(report.held.contains(&oldest), "{report:?}");
    assert!(exists(&env, &oldest));

    child.kill().unwrap();
    child.wait().unwrap();
    // Block until the OS has released the dead holder's lock, then collect.
    let mut probe = env.store.generation_lock(&oldest).unwrap();
    probe.lock(Mode::Exclusive, "the killed holder", &NullProgress).unwrap();
    probe.unlock().unwrap();
    let report = env.store.gc(&Fixed(vec![])).unwrap();
    assert!(report.deleted.contains(&oldest), "{report:?}");
    assert!(!exists(&env, &oldest));
}

#[skuld::test]
fn gc_keeps_the_parent_chain_of_a_reachable_layer(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let parent_v1 = build_expiring(&env, key(1), None, Some(t0() + days(90)));
    let parent_v1_ref = parent_v1.tip().layer_ref();
    let child = build_expiring(&env, key(2), Some(&parent_v1), Some(t0() + days(120)));
    let child_ref = child.tip().layer_ref();
    drop((parent_v1, child));

    // The parent expires and is rebuilt; the child's `current` still records the old parent generation.
    env.clock.set(t0() + days(85));
    let parent_v2 = build_expiring(&env, key(1), None, Some(t0() + days(85) + days(90)));
    assert_ne!(parent_v2.tip().layer_ref(), parent_v1_ref);
    drop(parent_v2);

    let report = env.store.gc(&Fixed(vec![vec![child_ref]])).unwrap();
    assert_eq!(report.deleted, vec![]);
    assert!(exists(&env, &parent_v1_ref), "the child's recorded parent must survive");
    // Without anything naming the child, `current` still keeps it (and so its parent) alive.
    let report = env.store.gc(&Fixed(vec![])).unwrap();
    assert_eq!(report.deleted, vec![]);
}

#[skuld::test]
fn gc_aborts_before_deleting_when_reachability_fails(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let [oldest, _middle, _newest] = three_generations(&env, 1);
    let error = env.store.gc(&Failing).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(
        exists(&env, &oldest),
        "nothing may be deleted when reachability is unknown"
    );
}

#[skuld::test]
fn gc_skips_a_key_with_a_live_builder_and_sweeps_a_dead_one(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let markers = home.join("markers");
    std::fs::create_dir(&markers).unwrap();
    let mut builder = start_builder(home, &key(1).to_string(), &markers);
    let live_staging = builder.expect_stdout("building ");

    // A dead builder's leftover, in a key nobody is building.
    let dead = env.dirs.layers().join(key(2).to_string()).join(".staging-dead");
    std::fs::create_dir_all(&dead).unwrap();
    std::fs::write(dead.join("half"), b"x").unwrap();

    let report = env.store.gc(&Fixed(vec![])).unwrap();
    assert_eq!(report.staging_removed, 1, "{report:?}");
    assert!(
        Path::new(&live_staging).is_dir(),
        "a live builder's staging dir must survive"
    );
    assert!(!dead.exists());
    assert!(
        !env.dirs.layers().join(key(2).to_string()).exists(),
        "an emptied key dir is removed"
    );

    builder.go();
    builder.expect_stdout("done ");
    assert!(builder.child.wait().unwrap().success());
    assert!(env.store.lookup(&key(1)).unwrap().is_some());
}

#[skuld::test]
fn lease_chain_refuses_a_chain_with_a_missing_link(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let parent = build_expiring(&env, key(1), None, None);
    let child = build_expiring(&env, key(2), Some(&parent), None);
    let child_ref = child.tip().layer_ref();
    let parent_dir = parent.tip().dir().path().to_owned();
    drop((parent, child));
    std::fs::remove_dir_all(parent_dir).unwrap();

    let guard = env.store.begin_record().unwrap();
    let error = env.store.lease_chain(&child_ref, &guard).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Store);
    assert!(error.message().contains(&key(1).to_string()), "{error}");
}

#[skuld::test]
fn lease_chain_works_for_an_expired_layer(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let layer = build_expiring(&env, key(1), None, Some(t0() + days(90)))
        .tip()
        .layer_ref();
    env.clock.set(t0() + days(200));
    let guard = env.store.begin_record().unwrap();
    let chain = env.store.lease_chain(&layer, &guard).unwrap();
    assert_eq!(
        chain.refs(),
        vec![layer],
        "an instance record may name a layer past its expiry"
    );
}

struct ChannelProgress(std::sync::Mutex<mpsc::Sender<String>>);

impl Progress for ChannelProgress {
    fn message(&self, text: &str) {
        self.0.lock().unwrap().send(text.to_owned()).unwrap();
    }
}

#[skuld::test]
fn record_guard_blocks_gc_until_dropped(#[fixture(anna_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let clock = Arc::new(FakeClock::new(t0()));
    let (messages, waiting) = mpsc::channel();
    let store = LayerStore::open(
        &dirs,
        clock.clone() as Arc<dyn Clock>,
        Arc::new(ChannelProgress(std::sync::Mutex::new(messages))),
    )
    .unwrap();
    let probe_env = Env {
        dirs: dirs.clone(),
        store,
        clock,
        progress: Arc::new(crate::test_support::fixtures::MemoryProgress::default()),
    };
    let [oldest, _middle, _newest] = three_generations(&probe_env, 1);
    while waiting.try_recv().is_ok() {}

    let guard = probe_env.store.begin_record().unwrap();
    std::thread::scope(|scope| {
        let env = &probe_env;
        let collector = scope.spawn(move || env.store.gc(&Fixed(vec![])).unwrap());
        let message = waiting.recv().unwrap();
        assert!(message.starts_with("waiting for"), "{message}");
        assert!(
            exists(env, &oldest),
            "gc must not delete while a record is being written"
        );
        drop(guard);
        let report = collector.join().unwrap();
        assert!(report.deleted.contains(&oldest));
    });
    assert!(!exists(&probe_env, &oldest));
}

#[skuld::test]
fn generation_dirs_and_noise_are_told_apart(#[fixture(anna_home)] home: &Path) {
    let env = env(home);
    let [oldest, middle, newest] = three_generations(&env, 1);
    let key_dir = env.dirs.layers().join(oldest.key.to_string());
    std::fs::create_dir(key_dir.join("not-a-generation")).unwrap();
    let report = env.store.gc(&Fixed(vec![vec![middle]])).unwrap();
    assert_eq!(report.deleted, vec![oldest]);
    let mut left = names(&key_dir);
    left.retain(|name| name.parse::<Generation>().is_ok());
    let mut expected = vec![middle.generation.to_string(), newest.generation.to_string()];
    expected.sort();
    assert_eq!(left, expected);
    assert!(
        key_dir.join("not-a-generation").is_dir(),
        "gc touches only generation directories"
    );
}
