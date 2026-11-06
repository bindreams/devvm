//! The engine over the QEMU backend and the Linux guest (`qemu_parts`), built without touching the user's directories,
//! environment or network.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::backend::fake::fake_spec;
use crate::backend::qemu::{QemuParts, qemu_parts};
use crate::config::Config;
use crate::contain::Lifetime;
use crate::engine::engine_tests::{engine_fixture, instance_in, memory};
use crate::engine::{DEFAULT_BOOT_BOUND, Engine, EngineParts, ListFilter, NoHooks};
use crate::error::ErrorKind;
use crate::exit::NullProgress;
use crate::guest::{BuildCtx, Guests};
use crate::host::symlink_metadata_if_present;
use crate::recipe::Source;
use crate::store::fetch::{Fetcher, Response, Resume, Transport};
use crate::store::layers::layers_tests::{key, request};
use crate::store::layers::{Built, SystemClock};
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::host::TestHost;
use crate::{Arch, Dirs, Error, GuestKind, Result};

/// Counts its requests and refuses each with a download error.
#[derive(Default)]
struct Refusing(AtomicUsize);

impl Refusing {
    fn gets(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl Transport for Refusing {
    fn get(&self, url: &str, _resume: Option<Resume>) -> Result<Response> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(Error::new(
            ErrorKind::Download,
            format!("no network in this test: {url}"),
        ))
    }
}

fn parts_over(dirs: &Dirs, transport: &Arc<Refusing>, progress: &Arc<MemoryProgress>) -> QemuParts {
    let fetcher = Arc::new(Fetcher::new(dirs.downloads(), transport.clone(), progress.clone()));
    qemu_parts(dirs, Config::default(), fetcher, &TestHost, progress.clone())
}

/// An engine over `dirs` whose backend and guests are `qemu_parts`.
fn qemu_engine(dirs: &Dirs, transport: &Arc<Refusing>, progress: &Arc<MemoryProgress>) -> Arc<Engine> {
    let QemuParts { backend, guests } = parts_over(dirs, transport, progress);
    Engine::with_parts(EngineParts {
        dirs: dirs.clone(),
        host: &TestHost,
        backend: Some(backend),
        guests,
        clock: Arc::new(SystemClock),
        progress: progress.clone(),
        hooks: Arc::new(NoHooks),
        boot_bound: DEFAULT_BOOT_BOUND,
    })
}

/// Lists, opens, reaps and removes the instance `name` through `engine`.
fn list_open_reap_and_remove(engine: &Arc<Engine>, name: &str) {
    assert_eq!(engine.list(ListFilter::All).unwrap().len(), 1);
    engine.instance(name).unwrap();
    engine.reap();
    engine.instance(name).unwrap().remove().unwrap();
    assert!(engine.list(ListFilter::All).unwrap().is_empty());
}

#[skuld::test]
fn the_engine_downloads_nothing_for_ls_open_remove_and_reap(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    instance_in(&fx, "foo", home);
    let (transport, progress) = (Arc::new(Refusing::default()), memory());
    let engine = qemu_engine(&fx.env.dirs, &transport, &progress);

    list_open_reap_and_remove(&engine, "foo");

    assert_eq!(transport.gets(), 0);
}

#[skuld::test]
fn a_transport_that_refuses_fails_only_the_command_that_downloads(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    instance_in(&fx, "foo", home);
    let (transport, progress) = (Arc::new(Refusing::default()), memory());
    let engine = qemu_engine(&fx.env.dirs, &transport, &progress);
    list_open_reap_and_remove(&engine, "foo");

    let recipe = crate::recipe::embedded::get("ubuntu-24.04").expect("the embedded Ubuntu recipe");
    let builder = engine.guests().builder(GuestKind::Linux).unwrap();
    let context = BuildCtx {
        dirs: fx.env.dirs.clone(),
        arch: Arch::Amd64,
        disk_size: 1 << 30,
        progress: Arc::new(NullProgress),
    };
    let error = fx
        .env
        .store
        .build(&request(key(2)), None, |staging| {
            let source: &Source = &recipe.source;
            Ok(Built::from_meta(&builder.build_base(source, &context, staging.dir)?))
        })
        .expect_err("the transport refuses");

    assert_eq!(error.kind(), ErrorKind::Download);
    assert!(error.message().contains("no network in this test"), "{error}");
    assert_eq!(transport.gets(), 1);
}

#[skuld::test]
fn qemu_parts_serve_a_linux_guest(#[fixture(unicode_home)] home: &Path) {
    let parts = parts_over(&Dirs::under(home), &Arc::new(Refusing::default()), &memory());
    parts.guests.os(GuestKind::Linux).expect("a Linux guest");
    parts.guests.builder(GuestKind::Linux).expect("a Linux builder");
}

/// Dirs for a host that cannot name its runtime directory; the other four are as `Dirs::under(home)` has them.
fn dirs_without_a_runtime_directory(home: &Path) -> Dirs {
    Dirs::from_lookup(
        Some(home.to_path_buf()),
        &|name| {
            let dir = match name {
                "XDG_CONFIG_HOME" => "config",
                "XDG_DATA_HOME" => "data",
                "XDG_CACHE_HOME" => "cache",
                "XDG_STATE_HOME" => "state",
                _ => return None,
            };
            Some(home.join(dir).into())
        },
        &|| Err(Error::new(ErrorKind::Config, "no runtime directory on this test host")),
    )
    .unwrap()
}

#[skuld::test]
fn an_engine_over_a_host_that_cannot_name_its_runtime_directory_lists_and_launches_with_that_error(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = engine_fixture(home, memory());
    instance_in(&fx, "foo", home);
    let (transport, progress) = (Arc::new(Refusing::default()), memory());
    let dirs = dirs_without_a_runtime_directory(home);
    let engine = qemu_engine(&dirs, &transport, &progress);

    assert_eq!(engine.list(ListFilter::All).unwrap().len(), 1);
    engine.reap();
    let told = progress.messages();
    assert!(
        told.iter()
            .any(|m| m.contains("no runtime directory on this test host")),
        "the reap does not name the cause: {told:?}"
    );

    let spec = fake_spec(home.join("disk.img"), home.join("serial.log"), Lifetime::OutlivesOwner);
    let error = engine
        .backend()
        .unwrap()
        .launch(&spec)
        .err()
        .expect("a launch has no runtime directory");
    assert!(
        error.message().contains("no runtime directory on this test host"),
        "{error}"
    );
    assert_eq!(transport.gets(), 0, "a launch that cannot work downloads nothing first");
}

#[skuld::test]
fn building_the_engine_and_listing_create_no_runtime_directory(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let (transport, progress) = (Arc::new(Refusing::default()), memory());
    let engine = qemu_engine(&dirs, &transport, &progress);
    let absent = |when: &str| {
        let run = dirs.run().unwrap();
        assert!(
            symlink_metadata_if_present(run).unwrap().is_none(),
            "{when}: {} exists",
            run.display()
        );
    };

    absent("after construction");
    engine.list(ListFilter::All).unwrap();
    absent("after list");
    engine.reap();
    absent("after reap");
}
