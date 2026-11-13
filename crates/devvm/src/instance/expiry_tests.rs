use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use secrecy::ExposeSecret;

use crate::backend::Backend;
use crate::engine::engine_tests::{
    EngineFixture, engine_fixture, memory, record_machine, running_machine, stub_fixture,
};
use crate::engine::keys::ensure_keys;
use crate::engine::{DEFAULT_BOOT_BOUND, Engine, EngineParts, ListFilter, NoHooks};
use crate::guest::Guests;
use crate::guest::fake::FakeGuests;
use crate::instance::dir::InstanceDir;
use crate::instance::registry::registry_tests::new_instance_on;
use crate::store::layers::layers_tests::{build_expiring, days, key, t0};
use crate::test_support::fixtures::{FakeClock, MemoryProgress, unicode_home};
use crate::test_support::host::TestHost;
use crate::test_support::stub_backend::Attach;
use crate::vm::Guest;
use crate::{Command, ErrorKind, Instance};

/// An engine over the fixture's state whose clock reads `now`, reaching VMs through `backend`.
fn engine_with(
    fx: &EngineFixture,
    now: SystemTime,
    progress: Arc<MemoryProgress>,
    backend: Option<Arc<dyn Backend>>,
    guests: Arc<dyn Guests>,
) -> Arc<Engine> {
    Engine::with_parts(EngineParts {
        dirs: fx.env.dirs.clone(),
        host: &TestHost,
        backend,
        guests,
        clock: Arc::new(FakeClock::new(now)),
        progress,
        hooks: Arc::new(NoHooks),
        boot_bound: DEFAULT_BOOT_BOUND,
    })
}

/// An engine with no VM backend: a call that reaches a VM fails without starting one.
fn engine_at(fx: &EngineFixture, now: SystemTime, progress: Arc<MemoryProgress>) -> Arc<Engine> {
    engine_with(fx, now, progress, None, Arc::new(FakeGuests))
}

/// The stopped, ownerless instance `name`, created from `created_from` on the layer `layer`, whose recorded expiry is
/// `expires`.
fn named_instance(
    fx: &EngineFixture,
    name: &str,
    created_from: &Path,
    layer: u8,
    expires: Option<SystemTime>,
) -> InstanceDir {
    let chain = build_expiring(&fx.env, key(layer), None, expires);
    let guard = fx.env.store.begin_record().unwrap();
    let mut new = new_instance_on(&chain, Some(name), fx.fake.storage());
    new.created_from = created_from.to_path_buf();
    fx.engine
        .registry()
        .create(new, &guard)
        .unwrap()
        .make_ownerless(&mut || Ok(()))
        .unwrap()
}

/// The stopped, ownerless instance `foo` whose recorded expiry is `expires`.
fn instance_expiring(fx: &EngineFixture, home: &Path, expires: Option<SystemTime>) -> InstanceDir {
    named_instance(fx, "foo", home, 2, expires)
}

/// The instance `foo`, recorded as running as this process with its SSH endpoint at the fixture's server, which trusts
/// its keys: what `run` and `reboot` reach. Its recorded expiry is `expires`, and the engine's clock reads `now`.
pub(crate) fn running_expiring(
    fx: &EngineFixture,
    home: &Path,
    expires: SystemTime,
    now: SystemTime,
    progress: Arc<MemoryProgress>,
) -> Instance {
    let dir = instance_expiring(fx, home, Some(expires));
    let keys = {
        let op = dir.op_exclusive().unwrap();
        ensure_keys(&dir, &op, &TestHost, &mut |_| {}).unwrap()
    };
    fx.server
        .install(&keys.client_public, keys.host_private.expose_secret());
    record_machine(&dir, &running_machine(home, fx.server.addr()));
    let engine = engine_with(fx, now, progress, Some(fx.fake.clone()), fx.guests.clone());
    Instance::open_in(&engine, "foo").unwrap()
}

/// The messages each call that reaches the guest, and `ls`, says before it fails (the engine has no backend), by call
/// name.
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
    let engine = engine_at(&fx, t0() + days(23), progress.clone());

    Instance::open_in(&engine, "foo").unwrap().start().unwrap_err();
    assert_eq!(
        progress.messages(),
        ["instance foo: the guest expired on 2027-02-04 (UTC)"]
    );
}

/// An expiry within an hour of midnight UTC is on another date in every zone that is not UTC.
#[skuld::test]
fn the_date_is_the_expiry_s_date_in_utc_near_midnight(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let late = "2027-03-01T23:30:00Z";
    let early = "2027-03-02T00:30:00Z";
    for (layer, (name, expires)) in [("late", late), ("early", early)].into_iter().enumerate() {
        let at: jiff::Timestamp = expires.parse().unwrap();
        let expires = SystemTime::from(at);
        named_instance(&fx, name, home, 2 + layer as u8, Some(expires));
        let progress = memory();
        let engine = engine_at(&fx, expires + days(1), progress.clone());

        Instance::open_in(&engine, name).unwrap().start().unwrap_err();

        // The date written in the literal itself, which no zone has moved.
        assert_eq!(
            progress.messages(),
            [format!(
                "instance {name}: the guest expired on {} (UTC)",
                expires_text(at)
            )]
        );
    }
}

/// `YYYY-MM-DD` of `at` as its own RFC 3339 text writes it.
fn expires_text(at: jiff::Timestamp) -> String {
    at.to_string()[..10].to_owned()
}

#[skuld::test]
fn ls_warns_once_for_each_listed_expired_instance_and_for_no_other(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let (past, ahead) = (Some(t0() + days(20)), Some(t0() + days(40)));
    named_instance(&fx, "old", home, 2, past);
    named_instance(&fx, "also-old", home, 3, past);
    named_instance(&fx, "fresh", home, 4, ahead);
    named_instance(&fx, "never", home, 5, None);
    named_instance(&fx, "elsewhere", &home.join("elsewhere"), 6, past);
    let skipped = named_instance(&fx, "skipped", home, 7, past);
    std::fs::remove_file(skipped.path().join("owner.json")).unwrap();
    let progress = memory();
    let engine = engine_at(&fx, t0() + days(30), progress.clone());

    engine.list(ListFilter::CreatedIn(home.to_path_buf())).unwrap();

    let mut messages = progress.messages();
    messages.sort();
    assert_eq!(
        messages,
        [
            "instance also-old: the guest expired on 2027-02-04 (UTC)",
            "instance old: the guest expired on 2027-02-04 (UTC)",
            "skipping instance skipped: its owner.json is missing",
        ]
    );
}

#[skuld::test]
fn start_of_an_instance_whose_machine_runs_warns_once(#[fixture(unicode_home)] home: &Path) {
    let (fx, stub, _) = stub_fixture(home, Attach::Running);
    let dir = instance_expiring(&fx, home, Some(t0() + days(20)));
    record_machine(&dir, &running_machine(home, fx.server.addr()));
    let progress = memory();
    let engine = engine_with(&fx, t0() + days(30), progress.clone(), Some(stub), Arc::new(FakeGuests));

    Instance::open_in(&engine, "foo").unwrap().start().unwrap();

    assert_eq!(progress.messages().len(), 1, "{:?}", progress.messages());
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
