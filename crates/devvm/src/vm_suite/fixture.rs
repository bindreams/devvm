//! Where the VM tests keep their files, and the engine over them.
//!
//! Every root is below a directory named [`UNICODE_DIR`] (a space, non-ASCII characters, a surrogate pair), so each path
//! devvm builds in these tests carries them. The directory is in `C:\Users` where the `unicode_base` group is on (the
//! literal location of a user's directories; a hosted runner is elevated and can make it), and in the temporary
//! directory where it is off.

use std::ffi::OsString;
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::backend::MachineRecord;
use crate::backend::qemu::qemu_parts;
use crate::backend::qemu::sidecar::QemuInstall;
use crate::config::Config;
use crate::engine::engine_tests::engine_with_bound;
use crate::engine::{DEFAULT_BOOT_BOUND, Engine, LaunchHooks, LaunchStep};
use crate::exit::StderrProgress;
use crate::host;
use crate::recipe::Recipe;
use crate::store::fetch::{Fetcher, HttpTransport};
use crate::test_groups::{Group, unicode_base};
use crate::test_harness::GUEST;
use crate::test_support::fixtures::UNICODE_DIR;
use crate::test_support::qemu::{qemu_sidecar, socket_dir, write_config};
use crate::{BuildOptions, Command, Dirs, Error, ExitStatus, Guest, Image, Instance, RunAs, StartOptions, Vm};

use crate::test_support::roots::{USERS, XdgRoots};

/// A process's directories, resolved: the roots of [`XdgRoots`] as the engine's [`Dirs`].
pub(super) struct Roots {
    xdg: XdgRoots,
    pub dirs: Dirs,
}

impl Roots {
    /// Makes the roots; `run` is `XDG_RUNTIME_DIR` ([`XdgRoots::new`]).
    pub(super) fn new(run: Option<&Path>) -> Result<Roots, String> {
        let xdg = XdgRoots::new(run)?;
        let dirs = Roots::resolve(&xdg, None)?;
        Ok(Roots { xdg, dirs })
    }

    /// These roots' directories with `run` as the runtime directory instead: the same stores, a runtime directory of
    /// their own.
    pub(super) fn dirs_with_run(&self, run: &Path) -> Result<Dirs, String> {
        Roots::resolve(&self.xdg, Some(run))
    }

    fn resolve(xdg: &XdgRoots, run: Option<&Path>) -> Result<Dirs, String> {
        let var = |name: &str| -> Option<OsString> {
            match (name, run) {
                ("XDG_RUNTIME_DIR", Some(run)) => Some(run.as_os_str().to_owned()),
                _ => xdg.var(name).map(|dir| dir.as_os_str().to_owned()),
            }
        };
        Dirs::from_lookup(Some(xdg.top.clone()), &var, &|| {
            Err(crate::Error::new(
                crate::ErrorKind::Config,
                "these roots name no runtime directory",
            ))
        })
        .map_err(|e| format!("cannot resolve the roots: {e}"))
    }
}

/// Counts the machines launched through an engine.
#[derive(Default)]
pub(super) struct Launches(AtomicUsize);

impl Launches {
    pub(super) fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl LaunchHooks for Launches {
    fn reached(&self, step: LaunchStep, _machine: &MachineRecord) {
        if step == LaunchStep::Spawned {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// An engine over `dirs` on the QEMU of `install`, whose launches `hooks` sees.
pub(super) fn engine_in(dirs: &Dirs, install: &QemuInstall, hooks: Arc<dyn LaunchHooks>) -> Arc<Engine> {
    write_config(
        dirs,
        &Config {
            qemu_path: Some(install.bin.clone()),
        },
    );
    let progress = Arc::new(StderrProgress);
    let transport = HttpTransport::new().expect("an HTTP client");
    let fetcher = Arc::new(Fetcher::new(dirs.downloads(), Arc::new(transport), progress.clone()));
    let parts = qemu_parts(dirs, fetcher, host::current(), progress.clone());
    engine_with_bound(
        dirs.clone(),
        host::current(),
        Some(parts.backend),
        parts.guests,
        progress,
        DEFAULT_BOOT_BOUND,
        hooks,
    )
}

/// The engine the tests of a guest share, with the roots it reads: its layer store holds the Ubuntu base once per test
/// process. A test that uses it boots a guest, so it runs alone among such tests (the `GUEST` label).
pub(super) struct VmEngine {
    pub engine: Arc<Engine>,
    /// The machines this engine has launched.
    pub launches: Arc<Launches>,
    pub roots: Roots,
}

#[skuld::fixture(scope = process, serial = GUEST, labels = [crate::test_harness::GUEST])]
pub(super) fn vm_engine(#[fixture(qemu_sidecar)] install: &Arc<QemuInstall>) -> Result<VmEngine, String> {
    let roots = Roots::new(Some(&socket_dir()))?;
    let launches = Arc::new(Launches::default());
    let engine = engine_in(&roots.dirs, install, launches.clone());
    Ok(VmEngine {
        engine,
        launches,
        roots,
    })
}

impl VmEngine {
    /// A recipe that starts from the Ubuntu base and runs `steps` ([`recipe_in`]), whose file would be in this engine's
    /// state directory.
    pub(super) fn recipe(&self, steps: &[&str]) -> Recipe {
        recipe_in(&self.roots.dirs.state, steps)
    }

    /// The Ubuntu base, built into this engine's store the first time in a test process and found there afterwards.
    pub(super) fn base(&self) -> Image {
        self.engine
            .build(&self.recipe(&[]), &BuildOptions::default())
            .unwrap_or_else(|e| panic!("cannot build the Ubuntu base: {e}"))
    }
}

/// A recipe file `dir/r.devvm.yaml` that starts from the embedded Ubuntu base and runs `steps`, each one step's YAML.
pub(super) fn recipe_in(dir: &Path, steps: &[&str]) -> Recipe {
    let steps = if steps.is_empty() {
        "steps: []
"
        .to_owned()
    } else {
        let lines: String = steps
            .iter()
            .map(|step| {
                format!(
                    "  - {step}
"
                )
            })
            .collect();
        format!(
            "steps:
{lines}"
        )
    };
    Recipe::parse(
        &format!(
            "from: ubuntu-24.04
{steps}"
        ),
        &dir.join("r.devvm.yaml"),
    )
    .unwrap()
}

/// What a test that boots a guest asks for: two processors and 2 GiB, like the contract's guests.
pub(super) fn guest_options() -> StartOptions {
    StartOptions {
        cpus: Some(2),
        memory: Some(2 << 30),
        ..StartOptions::default()
    }
}

/// What a test that runs several guests at once asks for: one processor and 1 GiB each, so that a 4 vCPU runner is not
/// loaded past the boot bound.
pub(super) fn small_options() -> StartOptions {
    StartOptions {
        cpus: Some(1),
        memory: Some(1 << 30),
        ..StartOptions::default()
    }
}

/// Runs `argv` in `guest`, which must exit 0, and returns its stdout.
pub(super) fn run_ok(guest: &dyn Guest, argv: &[&str]) -> String {
    run_ok_as(guest, RunAs::User, argv)
}

/// [`run_ok`] with the privilege `run_as`.
pub(super) fn run_ok_as(guest: &dyn Guest, run_as: RunAs, argv: &[&str]) -> String {
    let output = guest
        .run(&Command {
            run_as,
            ..Command::new(argv.iter().copied())
        })
        .unwrap_or_else(|e| panic!("cannot run {argv:?}: {e}"));
    assert_eq!(
        output.status,
        ExitStatus::Code(0),
        "{argv:?}: {}",
        output.stderr_lossy()
    );
    output.stdout_lossy().into_owned()
}

/// One guest, booted once per test process, for the tests that only look at what a command does in it. It runs until the
/// process ends.
pub(super) struct SharedGuest {
    pub vm: Vm,
    _image: Image,
}

#[skuld::fixture(scope = process, serial = GUEST, labels = [crate::test_harness::GUEST])]
pub(super) fn shared_guest(#[fixture(vm_engine)] engine: &VmEngine) -> Result<SharedGuest, String> {
    let image = engine.base();
    let vm = image
        .start(&guest_options())
        .map_err(|e| format!("cannot start the shared guest: {e}"))?;
    Ok(SharedGuest { vm, _image: image })
}

/// An instance made by `up`, removed when this is dropped: its machine outlives the process, so a test that fails
/// would leave it running.
pub(super) struct Up(Option<Instance>);

impl Up {
    pub(super) fn new(image: &Image, name: &str, opts: &StartOptions) -> Result<Up, Error> {
        image.up(name, opts).map(|instance| Up(Some(instance)))
    }

    /// Removes the instance now, and reports how that went.
    pub(super) fn remove(mut self) -> Result<(), Error> {
        self.0.take().expect("an instance until it is removed").remove()
    }
}

impl Deref for Up {
    type Target = Instance;

    fn deref(&self) -> &Instance {
        self.0.as_ref().expect("an instance until it is removed")
    }
}

impl Drop for Up {
    fn drop(&mut self) {
        if let Some(instance) = self.0.take() {
            let name = instance.name().to_owned();
            if let Err(e) = instance.remove() {
                eprintln!("cannot remove instance {name}: {e}");
            }
        }
    }
}

// No VM ===============================================================================================================

/// What a user's directories are: the four XDG variables name directories below the user's, and `devvm`'s own are in
/// them. With `unicode_base` on the base is the literal `C:\Users`, and this checks the roots are there, not in the
/// temporary directory.
#[skuld::test(labels = [])]
fn dirs_resolve_all_four_xdg_variables_under_the_users_unicode_root(#[fixture(unicode_base)] _group: &Group) {
    let roots = Roots::new(None).unwrap();
    let user = Path::new(USERS).join(UNICODE_DIR);

    for dir in [
        &roots.dirs.config,
        &roots.dirs.data,
        &roots.dirs.cache,
        &roots.dirs.state,
    ] {
        assert!(
            dir.starts_with(&user),
            "{} is not below {}",
            dir.display(),
            user.display()
        );
        assert_eq!(dir.file_name().unwrap(), "devvm", "{}", dir.display());
        std::fs::read_dir(dir.parent().unwrap()).unwrap_or_else(|e| panic!("{} was not made: {e}", dir.display()));
    }
}
