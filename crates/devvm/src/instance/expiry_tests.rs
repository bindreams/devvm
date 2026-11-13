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

/// The messages of one call of each kind that reaches the guest, or lists, named by the call: `start`, `run`, `run_to`,
/// `ssh_config`, `copy_in`, `copy_out`, `reboot` and `ls`. The instance is stopped and the engine has no backend, so
/// each call fails, and the messages are those said on the way.
fn messages_of_each_call(engine: &Arc<Engine>, progress: &MemoryProgress) -> Vec<(&'static str, Vec<String>)> {
    let instance = Instance::open_in(engine, "foo").unwrap();
    let mut seen = 0;
    let mut since = |progress: &MemoryProgress| {
        let all = progress.messages();
        let new = all[seen..].to_vec();
        seen = all.len();
        new
    };
    let mut calls = Vec::new();
    let mut record = |call, kind: ErrorKind, expected: ErrorKind, progress: &MemoryProgress| {
        assert_eq!(kind, expected, "{call}");
        calls.push((call, since(progress)));
    };
    let cmd = Command::new(["true"]);
    let kind = instance.start().unwrap_err().kind();
    record("start", kind, ErrorKind::NotSupported, progress);
    let kind = instance.run(&cmd).unwrap_err().kind();
    record("run", kind, ErrorKind::Unavailable, progress);
    let kind = instance
        .run_to(&cmd, &mut Vec::new(), &mut Vec::new())
        .unwrap_err()
        .kind();
    record("run_to", kind, ErrorKind::Unavailable, progress);
    let kind = instance.ssh_config().unwrap_err().kind();
    record("ssh_config", kind, ErrorKind::Unavailable, progress);
    let kind = instance.copy_in(Path::new("src"), "dest").unwrap_err().kind();
    record("copy_in", kind, ErrorKind::Unavailable, progress);
    let kind = instance.copy_out("src", Path::new("dest")).unwrap_err().kind();
    record("copy_out", kind, ErrorKind::Unavailable, progress);
    let kind = instance.reboot().unwrap_err().kind();
    record("reboot", kind, ErrorKind::Unavailable, progress);
    engine.list(ListFilter::All).unwrap();
    calls.push(("ls", since(progress)));
    calls
}

/// What every call of `messages_of_each_call` says when nothing is expired.
fn silent() -> Vec<(&'static str, Vec<String>)> {
    [
        "start",
        "run",
        "run_to",
        "ssh_config",
        "copy_in",
        "copy_out",
        "reboot",
        "ls",
    ]
    .into_iter()
    .map(|call| (call, Vec::new()))
    .collect()
}

#[skuld::test]
fn every_call_that_reaches_the_guest_warns_once_when_the_stored_expiry_is_past(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    instance_expiring(&fx, home, Some(t0() + days(20)));
    let progress = memory();
    let engine = engine_at(&fx, t0() + days(30), progress.clone());

    for (call, messages) in messages_of_each_call(&engine, &progress) {
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

    assert_eq!(messages_of_each_call(&engine, &progress), silent());
}

#[skuld::test]
fn no_warning_while_the_stored_expiry_is_ahead(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    instance_expiring(&fx, home, Some(t0() + days(20)));
    let progress = memory();
    let engine = engine_at(&fx, t0() + days(19), progress.clone());

    assert_eq!(messages_of_each_call(&engine, &progress), silent());
}

#[skuld::test]
fn a_warning_is_one_terse_progress_line_naming_the_date(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    instance_expiring(&fx, home, Some(t0() + days(20)));
    let progress = memory();
    // The expiry is 2027-01-15 + 20 days; the clock reads 3 days after it.
    let engine = engine_at(&fx, t0() + days(23), progress.clone());

    Instance::open_in(&engine, "foo").unwrap().start().unwrap_err();
    assert_eq!(
        progress.messages(),
        ["instance foo: the guest expired on 2027-02-04 (UTC)"]
    );
}

#[skuld::test]
fn an_expiry_that_is_now_is_past(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    instance_expiring(&fx, home, Some(t0() + days(20)));
    let progress = memory();
    let engine = engine_at(&fx, t0() + days(20), progress.clone());

    engine.list(ListFilter::All).unwrap();
    assert_eq!(
        progress.messages(),
        ["instance foo: the guest expired on 2027-02-04 (UTC)"]
    );
}
