//! QEMU for the tests: the sidecar, a socket path that fits `sun_path`, and a bare machine to talk QMP to.
#![cfg_attr(
    not(windows),
    allow(dead_code, reason = "used by the VM tests, which only Windows can run")
)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use crate::backend::qemu::args::escape_option_value;
use crate::backend::qemu::backend::QemuBackend;
use crate::backend::qemu::qmp::{QmpEndpoint, Reservation};
use crate::backend::qemu::sidecar::{LazyInstall, QemuInstall};
use crate::backend::qemu::storage::QemuStorage;
use crate::backend::{FileClaim, Storage};
use crate::config::Config;
use crate::exit::{NullProgress, StderrProgress};
use crate::host::{self, ProcessWatch, symlink_metadata_if_present};
use crate::random;
use crate::store::fetch::{Fetcher, HttpTransport};
use crate::store::layers::ChainLease;
use crate::test_groups::{Group, vm_group};
use crate::test_support::fixtures::MemoryProgress;
use crate::test_support::roots;
use crate::{Dirs, ErrorKind};

#[path = "qemu_tests.rs"]
mod qemu_tests;

/// The directory holding a QEMU build's `bin/` and `share/`. When set, the VM tests use it instead of downloading the
/// pinned build.
pub const QEMU_DIR_VAR: &str = "DEVVM_TEST_QEMU_DIR";

/// Writes `config` as `<config>/config.yaml` under `dirs`.
pub fn write_config(dirs: &Dirs, config: &Config) {
    let text = match &config.qemu_path {
        Some(path) => {
            let path = path.to_str().expect("a UTF-8 QEMU path");
            format!("qemu:\n  path: {}\n", serde_json::to_string(path).unwrap())
        }
        None => String::new(),
    };
    std::fs::create_dir_all(&dirs.config).expect("the config directory");
    std::fs::write(dirs.config.join("config.yaml"), text).expect("config.yaml");
}

/// The QEMU install the VM tests run. Fails naming what is missing; the opt-out is `vm_group::OPT_OUT`.
#[skuld::fixture(scope = process)]
pub fn qemu_sidecar(#[fixture(vm_group)] _group: &Group) -> Result<Arc<QemuInstall>, String> {
    let config = Config {
        qemu_path: std::env::var_os(QEMU_DIR_VAR).map(|dir| PathBuf::from(dir).join("bin")),
    };
    // The cache is the user's, so the download is shared; the configuration is this process's own.
    let mut dirs = Dirs::from_env().map_err(|e| format!("cannot locate devvm's directories: {e}"))?;
    let config_home = skuld::TempDir::new().map_err(|e| format!("cannot create a temp dir: {e}"))?;
    dirs.config = config_home.join("config");
    write_config(&dirs, &config);
    let transport = HttpTransport::new().map_err(|e| format!("cannot build the HTTP client: {e}"))?;
    let progress = Arc::new(StderrProgress);
    let fetcher = Arc::new(Fetcher::new(dirs.downloads(), Arc::new(transport), progress.clone()));
    LazyInstall::new(dirs, fetcher, progress, host::current())
        .get()
        .map_err(|e| {
            format!(
                "QEMU is not available for the VM tests: {e} (set {QEMU_DIR_VAR} to a QEMU build's directory, or {})",
                vm_group::OPT_OUT
            )
        })
}

/// A QMP socket path in a short, non-ASCII directory under the temp directory, reserved as a launch reserves one; the
/// files beside it are deleted on drop.
///
/// The directory is the shared [`socket_dir`]: a socket path is limited (`Host::socket_path_max`), which the per-test
/// directories of `unicode_home` exceed. Socket names are random, so tests do not collide, and the shared directory is
/// never removed, so no test can find it gone. Nobody holds the claim file: a test that wants a live machine holds it.
pub struct Sock {
    pub endpoint: QmpEndpoint,
    pub claim: FileClaim,
}

/// The directory control sockets go in during tests, if any fits a socket path (`Host::socket_path_max`); see
/// [`roots::socket_dir`].
fn socket_dir_that_fits() -> Option<PathBuf> {
    // Room for the deepest layout any test makes: a [`scratch_run`] below the directory, the `devvm` directory that an
    // engine over it makes in that (`Dirs::run`), and a socket in that.
    let fits = |dir: &Path| {
        QmpEndpoint::fits(
            &dir.join(scratch_name([0; SCRATCH_DRAW_BYTES])).join("devvm"),
            host::current().socket_path_max(),
        )
    };
    roots::socket_dir(&fits)
}

/// The shared directory control sockets go in during tests, created if need be (see [`socket_dir_that_fits`]).
pub fn socket_dir() -> PathBuf {
    let dir = socket_dir_that_fits().unwrap_or_else(|| {
        panic!(
            "the temporary directory {} is too long for a control socket",
            std::env::temp_dir().display()
        )
    });
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Makes `dir` as devvm makes a runtime directory: for the user alone, so that devvm accepts it as one. A directory made
/// any other way is owned by `BUILTIN\Administrators` when the tests run elevated, and devvm refuses it.
fn make_run_dir(dir: &Path) {
    host::current()
        .create_runtime_dir(dir)
        .unwrap_or_else(|e| panic!("cannot make the runtime directory {}: {e}", dir.display()));
}

/// The runtime directory of the tests that need only some directory to reserve names in, made once below [`socket_dir`].
pub fn socket_run() -> PathBuf {
    static RUN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    RUN.get_or_init(|| {
        let run = socket_dir().join("s");
        make_run_dir(&run);
        run
    })
    .clone()
}

/// A runtime directory of its own below the shared [`socket_dir`], removed with its contents when dropped, for a test
/// that reserves names or sweeps. Short, so that a socket path in it fits the host's limit.
pub struct ScratchRun(PathBuf);

impl ScratchRun {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchRun {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        let _ = std::fs::remove_file(scratch_owner(&self.0));
    }
}

/// The bytes a [`scratch_run`]'s name is the hex of.
const SCRATCH_DRAW_BYTES: usize = 3;

/// The name of the [`scratch_run`] drawn from `draw`: `r` and its hex.
fn scratch_name(draw: [u8; SCRATCH_DRAW_BYTES]) -> String {
    format!("r{}", hex::encode(draw))
}

/// `<run>.own`, beside the directory `run`: made new by whoever takes the name, so that no two take one.
fn scratch_owner(run: &Path) -> PathBuf {
    let mut name = run.as_os_str().to_owned();
    name.push(".own");
    PathBuf::from(name)
}

/// A [`ScratchRun`] below `parent` under a name drawn from `draw`. A name is taken by creating its `.own` file, which
/// only one test can do, and a name whose directory is already there (a killed run's leftover) is drawn again.
fn scratch_run_in(parent: &Path, draw: &mut dyn FnMut() -> [u8; SCRATCH_DRAW_BYTES]) -> ScratchRun {
    loop {
        let run = parent.join(scratch_name(draw()));
        let owner = scratch_owner(&run);
        match std::fs::File::create_new(&owner) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("cannot make {}: {e}", owner.display()),
        }
        let there = symlink_metadata_if_present(&run)
            .unwrap_or_else(|e| panic!("cannot look at {}: {e}", run.display()))
            .is_some();
        if there {
            let _ = std::fs::remove_file(&owner);
            continue;
        }
        make_run_dir(&run);
        return ScratchRun(run);
    }
}

pub fn scratch_run() -> ScratchRun {
    let run = scratch_run_in(&socket_dir(), &mut random::bytes::<SCRATCH_DRAW_BYTES>);
    assert!(
        QmpEndpoint::fits(run.path(), host::current().socket_path_max()),
        "{} is too long for a control socket",
        run.path().display()
    );
    run
}

/// `Err` when the socket directory lacks a space or a non-ASCII character, or none fits at all (the tests would check
/// nothing).
pub fn unicode_socket_dir_fits() -> Result<(), String> {
    let temp = std::env::temp_dir();
    match socket_dir_that_fits() {
        Some(dir) if roots::has_space_and_non_ascii(&dir) => Ok(()),
        found => Err(format!(
            "{} the temporary directory {} is {} bytes and a socket path is limited to {}",
            if found.is_some() {
                "no non-ASCII directory fits a socket path:"
            } else {
                "no directory fits a socket path:"
            },
            temp.display(),
            temp.as_os_str().len(),
            host::current().socket_path_max(),
        )),
    }
}

/// Asserts that a control socket path has a space and a non-ASCII character, as [`socket_dir`] provides where
/// [`unicode_socket_dir_fits`].
pub fn assert_unicode_socket_path(path: &Path) {
    assert!(roots::has_space_and_non_ascii(path), "{}", path.display());
}

impl Sock {
    pub fn new() -> Sock {
        let Reservation { endpoint, claim, .. } =
            QmpEndpoint::reserve(host::current(), &socket_run(), &NullProgress).unwrap();
        Sock { endpoint, claim }
    }
}

impl Drop for Sock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.endpoint.path());
        let _ = std::fs::remove_file(self.endpoint.lock_path());
        let _ = std::fs::remove_file(self.endpoint.hold_path());
    }
}

/// A QEMU with no machine (`-machine none`), frozen, listening for QMP, with one user-mode forward on port 0. No guest
/// and no accelerator: it needs only the sidecar. Killed on drop.
pub struct BareQemu {
    child: Child,
    pub sock: Sock,
    /// Reports when the process exits.
    pub process: Arc<dyn ProcessWatch>,
}

impl BareQemu {
    pub fn spawn(install: &QemuInstall, dir: &Path) -> BareQemu {
        let sock = Sock::new();
        let cwd = dir.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let log = std::fs::File::create(dir.join("qemu.log")).unwrap();
        let mut command = Command::new(install.tool("qemu-system-x86_64"));
        command
            .args([
                "-machine",
                "none",
                "-accel",
                "tcg",
                "-display",
                "none",
                "-nodefaults",
                "-no-user-config",
                "-S",
            ])
            .arg("-chardev")
            .arg(format!(
                "socket,id=qmp,path={},server=on,wait=off",
                escape_option_value(sock.endpoint.path()).unwrap()
            ))
            .args(["-object", "monitor-qmp,id=qmp0,chardev=qmp"])
            .args(["-netdev", "user,id=n0,hostfwd=tcp:127.0.0.1:0-:22"])
            .current_dir(cwd)
            .env_clear()
            .envs(install.environment(host::current().system_dirs().as_ref()).unwrap())
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        host::current().quiet(&mut command);
        let child = command
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {}: {e}", install.tool("qemu-system-x86_64").display()));
        let pid = child.id();
        let process = match cosca::Process::from_pid(pid) {
            cosca::identity::Resolved::Found(process) => host::current().watch_process(process.id()).unwrap(),
            other => panic!("cannot identify QEMU {pid} right after starting it: {other:?}"),
        };
        BareQemu { child, sock, process }
    }
}

impl Drop for BareQemu {
    fn drop(&mut self) {
        // This test's own child, by handle.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A `LazyInstall` that resolves to `install` without downloading (its tools are used in place).
pub fn lazy_for(install: &QemuInstall) -> Arc<LazyInstall> {
    lazy_at(&install.bin)
}

/// A `LazyInstall` over the QEMU whose executables are in `bin`. Its configuration is read here, from a `config.yaml`
/// in a directory of its own that is gone when this returns.
pub fn lazy_at(bin: &Path) -> Arc<LazyInstall> {
    let home = skuld::TempDir::new().expect("a temp dir");
    let dirs = Dirs::under(&home);
    write_config(
        &dirs,
        &Config {
            qemu_path: Some(bin.to_path_buf()),
        },
    );
    let progress = Arc::new(StderrProgress);
    let transport = HttpTransport::new().expect("an HTTP client");
    let fetcher = Arc::new(Fetcher::new(dirs.downloads(), Arc::new(transport), progress.clone()));
    let install = Arc::new(LazyInstall::new(dirs, fetcher, progress, host::current()));
    install.config().expect("the config just written");
    install
}

/// A QEMU backend that reserves and sweeps in `run` and has no QEMU to launch.
pub fn backend_in(run: &Path, progress: Arc<MemoryProgress>) -> Arc<QemuBackend> {
    let install = lazy_at(Path::new("no-qemu-here"));
    let storage = Arc::new(QemuStorage::new(install.clone(), host::current()));
    Arc::new(QemuBackend::new(
        install,
        storage,
        host::current(),
        Ok(run.to_path_buf()),
        progress,
    ))
}

/// Runs the sidecar's `qemu-img` and returns its stdout; panics with its stderr when it fails.
pub fn qemu_img(install: &QemuInstall, args: &[&OsStr]) -> Vec<u8> {
    let output = install
        .command("qemu-img", host::current())
        .unwrap()
        .args(args)
        .output()
        .expect("qemu-img should run");
    assert!(
        output.status.success(),
        "qemu-img {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// `qemu-img info --output=json` of `image`; with `chain`, of its whole backing chain (an array).
pub fn qemu_img_info(install: &QemuInstall, image: &Path, chain: bool) -> serde_json::Value {
    let mut args: Vec<&OsStr> = vec!["info".as_ref(), "--output=json".as_ref()];
    if chain {
        args.push("--backing-chain".as_ref());
    }
    args.push(image.as_os_str());
    serde_json::from_slice(&qemu_img(install, &args)).expect("qemu-img info prints JSON")
}

/// The refusal of `storage.create_instance_disk` of `size` on `base`, as an error of `kind`, after checking that no disk
/// and no firmware variables were made.
pub fn refused_without_a_disk(
    storage: &QemuStorage,
    base: &ChainLease,
    home: &Path,
    size: Option<u64>,
    kind: ErrorKind,
) -> crate::Error {
    let size_name = size.map_or("none".to_owned(), |size| size.to_string());
    let instance = home.join(format!("refused-{size_name}")).join("disk.img");
    std::fs::create_dir_all(instance.parent().unwrap()).unwrap();

    let error = storage
        .create_instance_disk(base.tip().dir(), &instance, size)
        .unwrap_err();

    assert_eq!(error.kind(), kind, "{size_name}: {error}");
    assert!(
        matches!(symlink_metadata_if_present(&instance), Ok(None)),
        "{size_name}: a disk was made"
    );
    assert!(
        matches!(
            symlink_metadata_if_present(&instance.parent().unwrap().join(crate::backend::qemu::storage::VARS)),
            Ok(None)
        ),
        "{size_name}: the firmware variables were copied"
    );
    error
}

/// The bytes a guest would read from `disk` (through its backing chain), as a raw image.
pub fn read_through(install: &QemuInstall, disk: &Path, scratch: &Path) -> Vec<u8> {
    let raw = scratch.join("read-through.raw");
    qemu_img(
        install,
        &[
            "convert".as_ref(),
            "-O".as_ref(),
            "raw".as_ref(),
            disk.as_os_str(),
            raw.as_os_str(),
        ],
    );
    let bytes = std::fs::read(&raw).unwrap();
    std::fs::remove_file(raw).unwrap();
    bytes
}
