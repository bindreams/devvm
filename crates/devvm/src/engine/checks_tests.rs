use std::path::Path;
use std::sync::Arc;

use super::{host_checks, tool_check};
use crate::backend::fake::FakeBackend;
use crate::backend::{Backend, Check};
use crate::engine::Engine;
use crate::engine::engine_tests::{engine_with, memory};
use crate::guest::fake::FakeGuests;
use crate::host::Host;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::host::{NO_ACCELERATOR, NoAccelHost, RecordingHost, TestHost};
use crate::{Dirs, Unavailable};

fn passing(name: &str) -> Check {
    Check {
        name: name.to_owned(),
        outcome: Ok(format!("{name} is fine")),
    }
}

fn failing(name: &str, why: &str) -> Check {
    Check {
        name: name.to_owned(),
        outcome: Err(why.to_owned()),
    }
}

/// An engine on `host` over a fake backend whose checks are `checks`, or over no backend.
fn engine_checking(home: &Path, host: &'static dyn Host, checks: Option<Vec<Check>>) -> Arc<Engine> {
    let backend = checks.map(|checks| Arc::new(FakeBackend::new(&TestHost).with_checks(checks)) as Arc<dyn Backend>);
    engine_with(Dirs::under(home), host, backend, Arc::new(FakeGuests), memory())
}

fn missing(result: Result<(), Unavailable>) -> String {
    result.expect_err("something is missing").missing
}

#[skuld::test]
fn availability_names_the_missing_piece_for_each_cause(#[fixture(unicode_home)] home: &Path) {
    // No accelerator.
    let engine = engine_checking(home, &NoAccelHost, Some(vec![passing("qemu")]));
    assert_eq!(missing(engine.availability()), NO_ACCELERATOR);

    // The backend lacks something.
    let engine = engine_checking(
        home,
        &TestHost,
        Some(vec![passing("one"), failing("qemu", "QEMU is gone")]),
    );
    assert_eq!(missing(engine.availability()), "QEMU is gone");

    // The accelerator is named before the backend's pieces.
    let engine = engine_checking(home, &NoAccelHost, Some(vec![failing("qemu", "QEMU is gone")]));
    assert_eq!(missing(engine.availability()), NO_ACCELERATOR);

    // No backend in this build.
    let engine = engine_checking(home, &TestHost, None);
    assert!(missing(engine.availability()).contains("no VM backend"));

    // Nothing is missing.
    let engine = engine_checking(home, &TestHost, Some(vec![passing("qemu")]));
    engine.availability().expect("everything passes");
}

#[skuld::test]
fn availability_ignores_what_only_doctor_reports(#[fixture(unicode_home)] home: &Path) {
    let host: &'static RecordingHost = Box::leak(Box::new(RecordingHost::new()));
    let dirs = Dirs::under(home);
    host.tell_free_space(&dirs.data, Err("the disk is gone"));
    let engine = engine_with(
        dirs,
        host,
        Some(Arc::new(FakeBackend::new(&TestHost))),
        Arc::new(FakeGuests),
        memory(),
    );

    engine
        .availability()
        .expect("free space is a doctor line, not availability");
}

#[skuld::test]
fn the_host_checks_name_the_accelerator_and_the_free_space_of_the_data_and_cache_directories(
    #[fixture(unicode_home)] home: &Path,
) {
    let host = RecordingHost::new();
    let dirs = Dirs::under(home);
    host.tell_free_space(&dirs.data, Ok(5 << 30));
    host.tell_free_space(&dirs.cache, Err("the disk is gone"));

    let checks = host_checks(&host, &dirs);

    let names: Vec<_> = checks.iter().map(|check| check.name.as_str()).collect();
    assert_eq!(names, ["accelerator", "free space (data)", "free space (cache)"]);
    assert_eq!(checks[0].outcome, Ok("tcg".to_owned()));
    let data = checks[1].outcome.as_ref().expect("the data directory has room");
    assert!(data.contains(&bytesize::ByteSize(5 << 30).to_string()), "{data}");
    assert!(data.contains(&dirs.data.display().to_string()), "{data}");
    let cache = checks[2]
        .outcome
        .as_ref()
        .expect_err("the cache directory cannot be asked");
    assert!(cache.contains("the disk is gone"), "{cache}");
    assert!(cache.contains(&dirs.cache.display().to_string()), "{cache}");
}

#[skuld::test]
fn a_host_without_an_accelerator_fails_the_accelerator_line_with_what_is_missing(#[fixture(unicode_home)] home: &Path) {
    let checks = host_checks(&NoAccelHost, &Dirs::under(home));

    assert_eq!(checks[0].name, "accelerator");
    assert_eq!(checks[0].outcome, Err(NO_ACCELERATOR.to_owned()));
}

// These start a process, so they do not run on the owner's PC; CI and the deputy's runner do.

#[skuld::test]
fn a_tool_that_runs_passes_with_what_it_printed_and_one_that_does_not_exist_fails() {
    let git = tool_check(&TestHost, "git", &["--version"]);
    assert_eq!(git.name, "git");
    let printed = git.outcome.expect("the tests need git");
    assert!(printed.starts_with("git version"), "{printed}");

    let absent = tool_check(&TestHost, "devvm-no-such-tool-4f1c", &["--version"]);
    let why = absent.outcome.expect_err("there is no such tool");
    assert!(why.contains("devvm-no-such-tool-4f1c"), "{why}");
}

#[skuld::test]
fn the_engine_lists_the_host_checks_then_the_tools_then_the_backends(#[fixture(unicode_home)] home: &Path) {
    let engine = engine_checking(home, &TestHost, Some(vec![passing("qemu")]));

    let names: Vec<_> = engine.checks().into_iter().map(|check| check.name).collect();

    assert_eq!(
        names,
        [
            "accelerator",
            "free space (data)",
            "free space (cache)",
            "ssh",
            "git",
            "qemu"
        ]
    );
}
