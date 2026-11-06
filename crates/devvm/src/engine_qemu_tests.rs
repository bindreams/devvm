//! The engine over the QEMU backend and the Linux guest (`qemu_parts`), built without touching the user's directories,
//! environment or network. Every test drives the engine; the one fixture is an engine over those parts whose transport
//! refuses every download, on state that holds one stopped instance, `foo`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::backend::fake::fake_spec;
use crate::backend::qemu::sidecar::manifest_entry;
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
use crate::test_support::qemu::{scratch_run, socket_dir};
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
    /// An engine over `dirs`, whose config, data, cache and state directories are below `home`; its runtime directory
    /// can be anywhere.
    fn over(home: &Path, dirs: Dirs) -> QemuEngine {
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
        QemuEngine::over(home, Dirs::under(home))
    }

    /// An engine over the layout of `Dirs::under(home)` except for the runtime directory: `run` is where the host
    /// puts it, or why it cannot name one.
    fn with_run(home: &Path, run: Result<PathBuf>) -> QemuEngine {
        let vars = ["config", "data", "cache", "state"].map(|name| {
            (
                format!("XDG_{}_HOME", name.to_uppercase()),
                home.join(name).into_os_string(),
            )
        });
        let var = |name: &str| vars.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
        let dirs = Dirs::from_lookup(Some(home.to_path_buf()), &var, &|| run.clone()).unwrap();
        QemuEngine::over(home, dirs)
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

#[skuld::test]
fn a_launch_resolves_qemu_through_the_transport_it_was_given(#[fixture(unicode_home)] home: &Path) {
    // `Dirs::under(home)` can be too long for a socket path: the launch refuses that before it downloads.
    let run = scratch_run();
    let qemu = QemuEngine::with_run(home, Ok(run.path().to_path_buf()));

    let error = qemu.launch();

    // A host the manifest has a build for downloads it; any other fails naming its triple, before the transport.
    if manifest_entry(crate::TARGET).is_ok() {
        assert_eq!(error.kind(), ErrorKind::Download, "{error}");
        assert_eq!(qemu.downloads().len(), 1, "{:?}", qemu.downloads());
    } else {
        assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
        assert!(error.message().contains(crate::TARGET), "{error}");
        assert!(qemu.downloads().is_empty(), "{:?}", qemu.downloads());
    }
}

#[skuld::test]
fn the_engine_serves_a_linux_guest(#[fixture(unicode_home)] home: &Path) {
    let qemu = QemuEngine::new(home);

    qemu.engine.guests().os(GuestKind::Linux).expect("a Linux guest");
    qemu.engine.guests().builder(GuestKind::Linux).expect("a Linux builder");
}

#[skuld::test]
fn without_a_runtime_directory_the_reap_warns_and_a_launch_fails_naming_it(#[fixture(unicode_home)] home: &Path) {
    let qemu = QemuEngine::with_run(home, Err(Error::new(ErrorKind::Config, RUN_DIR_ERROR)));

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

#[skuld::test]
fn a_launch_over_a_runtime_path_it_refuses_downloads_nothing(#[fixture(unicode_home)] home: &Path) {
    // Beside the socket directory's own scratch runtime directories, so that the path is as short as theirs: a path
    // below `home` can be too long for a socket, which a launch refuses first. Named `f...`, never a scratch run's
    // `r...` name, so it cannot take one (`scratch_run_in`).
    let file = tempfile::Builder::new()
        .prefix("f")
        .tempfile_in(socket_dir())
        .expect("a file for a runtime path");
    let too_long = home.join("x".repeat(crate::host::current().socket_path_max()));
    let cases = [
        (too_long, ErrorKind::Unavailable, "XDG_RUNTIME_DIR"),
        (file.path().to_path_buf(), ErrorKind::Io, "is not a directory"),
    ];

    for (n, (run, kind, fragment)) in cases.into_iter().enumerate() {
        let qemu = QemuEngine::with_run(&home.join(n.to_string()), Ok(run.clone()));

        let error = qemu.launch();

        assert_eq!(error.kind(), kind, "{}: {error}", run.display());
        assert!(error.message().contains(fragment), "{}: {error}", run.display());
        assert!(qemu.downloads().is_empty(), "{:?}", qemu.downloads());
    }
}
