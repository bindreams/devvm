use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use crate::backend::Backend;
use crate::engine::engine_tests::{EngineFixture, engine_fixture, memory};
use crate::engine::{DEFAULT_BOOT_BOUND, Engine, EngineParts, ListFilter, NoHooks};
use crate::guest::fake::FakeGuests;
use crate::instance::registry::registry_tests::new_instance_on;
use crate::store::layers::layers_tests::{build_expiring, days, key, t0};
use crate::test_support::fixtures::{FakeClock, MemoryProgress, unicode_home};
use crate::test_support::host::TestHost;
use crate::vm::Guest;
use crate::{Command, ErrorKind, Instance};

/// An engine over the fixture's state whose clock reads `now`, with no VM backend: a call that reaches a VM fails
/// without starting one.
fn engine_at(fx: &EngineFixture, now: SystemTime, progress: Arc<MemoryProgress>) -> Arc<Engine> {
    Engine::with_parts(EngineParts {
        dirs: fx.env.dirs.clone(),
        host: &TestHost,
        backend: None::<Arc<dyn Backend>>,
        guests: Arc::new(FakeGuests),
        clock: Arc::new(FakeClock::new(now)),
        progress,
        hooks: Arc::new(NoHooks),
        boot_bound: DEFAULT_BOOT_BOUND,
    })
}

/// The stopped, ownerless instance `foo` whose recorded expiry is `expires`.
fn instance_expiring(fx: &EngineFixture, home: &Path, expires: Option<SystemTime>) {
    let chain = build_expiring(&fx.env, key(2), None, expires);
    let guard = fx.env.store.begin_record().unwrap();
    let mut new = new_instance_on(&chain, Some("foo"), fx.fake.storage());
    new.created_from = home.to_path_buf();
    fx.engine
        .registry()
        .create(new, &guard)
        .unwrap()
        .make_ownerless(&mut || Ok(()))
        .unwrap();
}

/// The messages of one call of each kind that the warning belongs to, in the order `start`, `run`, `run_to`, `ls`.
fn messages_of_each_call(engine: &Arc<Engine>, progress: &MemoryProgress) -> [Vec<String>; 4] {
    let instance = Instance::open_in(engine, "foo").unwrap();
    let mut seen = 0;
    let mut since = |progress: &MemoryProgress| {
        let all = progress.messages();
        let new = all[seen..].to_vec();
        seen = all.len();
        new
    };
    let start = instance.start().unwrap_err();
    assert_eq!(start.kind(), ErrorKind::NotSupported);
    let started = since(progress);
    let run = instance.run(&Command::new(["true"])).unwrap_err();
    assert_eq!(run.kind(), ErrorKind::Unavailable);
    let ran = since(progress);
    let run_to = instance
        .run_to(&Command::new(["true"]), &mut Vec::new(), &mut Vec::new())
        .unwrap_err();
    assert_eq!(run_to.kind(), ErrorKind::Unavailable);
    let ran_to = since(progress);
    engine.list(ListFilter::All).unwrap();
    [started, ran, ran_to, since(progress)]
}

#[skuld::test]
fn start_run_and_ls_warn_once_when_the_stored_expiry_is_past(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    instance_expiring(&fx, home, Some(t0() + days(20)));
    let progress = memory();
    let engine = engine_at(&fx, t0() + days(30), progress.clone());

    for (call, messages) in ["start", "run", "run_to", "ls"]
        .iter()
        .zip(messages_of_each_call(&engine, &progress))
    {
        assert_eq!(messages.len(), 1, "{call}: {messages:?}");
        assert!(
            messages[0].starts_with("instance foo: the guest expired on "),
            "{call}: {messages:?}"
        );
    }
}

#[skuld::test]
fn no_warning_without_an_expiry(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    instance_expiring(&fx, home, None);
    let progress = memory();
    let engine = engine_at(&fx, t0() + days(3650), progress.clone());

    assert_eq!(
        messages_of_each_call(&engine, &progress),
        [vec![], vec![], vec![], vec![]] as [Vec<String>; 4]
    );
}

#[skuld::test]
fn no_warning_while_the_stored_expiry_is_ahead(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    instance_expiring(&fx, home, Some(t0() + days(20)));
    let progress = memory();
    let engine = engine_at(&fx, t0() + days(19), progress.clone());

    assert_eq!(
        messages_of_each_call(&engine, &progress),
        [vec![], vec![], vec![], vec![]] as [Vec<String>; 4]
    );
}

#[skuld::test]
fn a_warning_is_one_terse_progress_line_naming_the_date(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    instance_expiring(&fx, home, Some(t0() + days(20)));
    let progress = memory();
    // The expiry is 2027-01-15 + 20 days; the clock reads 3 days after it.
    let engine = engine_at(&fx, t0() + days(23), progress.clone());

    Instance::open_in(&engine, "foo").unwrap().start().unwrap_err();
    assert_eq!(progress.messages(), ["instance foo: the guest expired on 2027-02-04"]);
}

#[skuld::test]
fn an_expiry_that_is_now_is_past(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    instance_expiring(&fx, home, Some(t0() + days(20)));
    let progress = memory();
    let engine = engine_at(&fx, t0() + days(20), progress.clone());

    engine.list(ListFilter::All).unwrap();
    assert_eq!(progress.messages(), ["instance foo: the guest expired on 2027-02-04"]);
}
