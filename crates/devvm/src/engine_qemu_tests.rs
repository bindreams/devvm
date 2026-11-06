//! The engine over the QEMU backend and the Linux guest (`qemu_parts`), built without touching the user's directories,
//! environment or network. Every test drives the engine; the one fixture is an engine over those parts whose transport
//! refuses every download, on state that holds one stopped instance, `foo`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::backend::fake::fake_spec;
use crate::backend::qemu::{QemuParts, qemu_parts};
use crate::config::Config;
use crate::contain::Lifetime;
use crate::engine::engine_tests::{EngineFixture, engine_fixture, engine_with, instance_in, memory};
use crate::engine::{Engine, ListFilter};
use crate::error::ErrorKind;
use crate::exit::NullProgress;
use crate::guest::BuildCtx;
use crate::host::symlink_metadata_if_present;
use crate::store::fetch::Fetcher;
use crate::store::layers::Built;
use crate::store::layers::layers_tests::{key, request};
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::host::TestHost;
use crate::test_support::transport::RefusingTransport;
use crate::{Arch, Dirs, Error, GuestKind, Result};

const RUN_DIR_ERROR: &str = "no runtime directory on this test host";

struct QemuEngine {
    engine: Arc<Engine>,
    home: PathBuf,
    dirs: Dirs,
    transport: Arc<RefusingTransport>,
    progress: Arc<MemoryProgress>,
    /// Keeps the state the engine reads: the store, and the instance `foo` in it.
    _state: EngineFixture,
}

impl QemuEngine {
    /// An engine over `Dirs::under(home)` with `run` as the runtime directory, or the reason the host cannot name it.
    fn over(home: &Path, run: Result<PathBuf>) -> QemuEngine {
        let dirs = Dirs::under_with_run(home, run);
        let state = engine_fixture(home, memory());
        instance_in(&state, "foo", home);
        let (transport, progress) = (Arc::new(RefusingTransport::default()), memory());
        let fetcher = Arc::new(Fetcher::new(dirs.downloads(), transport.clone(), progress.clone()));
        let QemuParts { backend, guests } = qemu_parts(&dirs, Config::default(), fetcher, &TestHost, progress.clone());
        let engine = engine_with(dirs.clone(), &TestHost, Some(backend), guests, progress.clone());
        QemuEngine {
            engine,
            home: home.to_path_buf(),
            dirs,
            transport,
            progress,
            _state: state,
        }
    }

    fn new(home: &Path) -> QemuEngine {
        QemuEngine::over(home, Dirs::under(home).run().map(Path::to_path_buf))
    }

    /// The URLs the engine has asked its transport for.
    fn downloads(&self) -> Vec<String> {
        self.transport.asked()
    }

    /// The error of the engine's Linux guest building a base layer, which downloads the base image.
    fn base_build(&self) -> Error {
        let recipe = crate::recipe::embedded::get("ubuntu-24.04").expect("the embedded Ubuntu recipe");
        let builder = self.engine.guests().builder(GuestKind::Linux).unwrap();
        let context = BuildCtx {
            dirs: self.dirs.clone(),
            arch: Arch::Amd64,
            disk_size: 1 << 30,
            progress: Arc::new(NullProgress),
        };
        self.engine
            .store()
            .unwrap()
            .build(&request(key(2)), None, |staging| {
                Ok(Built::from_meta(&builder.build_base(
                    &recipe.source,
                    &context,
                    staging.dir,
                )?))
            })
            .expect_err("the transport refuses")
    }

    /// The error of the engine's backend launching a machine. (`Instance::start` stops at the seed's placeholder
    /// keys before it reaches the backend.)
    fn launch(&self) -> Error {
        let spec = fake_spec(
            self.home.join("disk.img"),
            self.home.join("serial.log"),
            Lifetime::OutlivesOwner,
        );
        self.engine
            .backend()
            .unwrap()
            .launch(&spec)
            .err()
            .expect("a launch needs QEMU, which the transport refuses, or a runtime directory")
    }
}

#[skuld::test]
fn a_transport_that_refuses_fails_only_the_command_that_downloads(#[fixture(unicode_home)] home: &Path) {
    let qemu = QemuEngine::new(home);
    let engine = &qemu.engine;

    assert_eq!(engine.list(ListFilter::All).unwrap().len(), 1);
    engine.instance("foo").unwrap();
    engine.reap();
    assert!(qemu.downloads().is_empty(), "{:?}", qemu.downloads());

    let error = qemu.base_build();
    assert_eq!(error.kind(), ErrorKind::Download, "{error}");
    assert!(error.message().contains("no network in this test"), "{error}");
    assert_eq!(qemu.downloads().len(), 1, "{:?}", qemu.downloads());

    engine.instance("foo").unwrap().remove().unwrap();
    assert!(engine.list(ListFilter::All).unwrap().is_empty());
}

// The manifest publishes QEMU for Windows only; elsewhere a launch fails `Unavailable` before it reaches the transport.
#[cfg(windows)]
#[skuld::test]
fn a_launch_resolves_qemu_through_the_transport_it_was_given(#[fixture(unicode_home)] home: &Path) {
    let qemu = QemuEngine::new(home);

    let error = qemu.launch();

    assert_eq!(error.kind(), ErrorKind::Download, "{error}");
    assert_eq!(qemu.downloads().len(), 1, "{:?}", qemu.downloads());
}

#[skuld::test]
fn the_engine_serves_a_linux_guest(#[fixture(unicode_home)] home: &Path) {
    let qemu = QemuEngine::new(home);

    qemu.engine.guests().os(GuestKind::Linux).expect("a Linux guest");
    qemu.engine.guests().builder(GuestKind::Linux).expect("a Linux builder");
}

#[skuld::test]
fn without_a_runtime_directory_the_reap_warns_and_a_launch_fails_naming_it(#[fixture(unicode_home)] home: &Path) {
    let qemu = QemuEngine::over(home, Err(Error::new(ErrorKind::Config, RUN_DIR_ERROR)));

    assert_eq!(qemu.engine.list(ListFilter::All).unwrap().len(), 1);
    qemu.engine.reap();
    let told = qemu.progress.messages();
    assert!(
        told.iter().any(|m| m.contains(RUN_DIR_ERROR)),
        "the reap does not name the cause: {told:?}"
    );

    let error = qemu.launch();
    assert!(error.message().contains(RUN_DIR_ERROR), "{error}");
    assert!(
        qemu.downloads().is_empty(),
        "a launch that cannot work downloads nothing first: {:?}",
        qemu.downloads()
    );
}

#[skuld::test]
fn building_the_engine_and_listing_create_no_runtime_directory(#[fixture(unicode_home)] home: &Path) {
    let qemu = QemuEngine::new(home);
    let absent = |when: &str| {
        let run = qemu.dirs.run().unwrap();
        assert!(
            symlink_metadata_if_present(run).unwrap().is_none(),
            "{when}: {} exists",
            run.display()
        );
    };

    absent("after construction");
    qemu.engine.list(ListFilter::All).unwrap();
    absent("after list");
    qemu.engine.reap();
    absent("after reap");
}
