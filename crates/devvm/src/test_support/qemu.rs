//! QEMU for the tests: the sidecar, a socket path that fits `sun_path`, and a bare machine to talk QMP to.
#![cfg_attr(
    not(windows),
    allow(dead_code, reason = "used by the VM tests, which only Windows can run")
)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use crate::Dirs;
use crate::backend::qemu::qmp::QmpEndpoint;
use crate::backend::qemu::sidecar::{LazyInstall, QemuInstall};
use crate::config::Config;
use crate::exit::StderrProgress;
use crate::host;
use crate::instance::record::InstanceId;
use crate::store::fetch::{Fetcher, HttpTransport};
use crate::test_support::fixtures::UNICODE_DIR;
use crate::vm_gate::{VM, vm_tests_enabled};

/// The directory holding a QEMU build's `bin/` and `share/`. When set, the VM tests use it instead of downloading the
/// pinned build.
pub const QEMU_DIR_VAR: &str = "DEVVM_TEST_QEMU_DIR";

/// The QEMU install the VM tests run. Fails naming what is missing; the opt-out is `DEVVM_SKIP_VM_TESTS=1`.
#[skuld::fixture(scope = process, requires = [vm_tests_enabled], labels = [VM])]
pub fn qemu_sidecar() -> Result<Arc<QemuInstall>, String> {
    let config = Config {
        qemu_path: std::env::var_os(QEMU_DIR_VAR).map(|dir| PathBuf::from(dir).join("bin")),
    };
    let dirs = Dirs::from_env().map_err(|e| format!("cannot locate devvm's directories: {e}"))?;
    let transport = HttpTransport::new().map_err(|e| format!("cannot build the HTTP client: {e}"))?;
    let progress = Arc::new(StderrProgress);
    let fetcher = Arc::new(Fetcher::new(dirs.downloads(), Arc::new(transport), progress.clone()));
    LazyInstall::new(dirs, config, fetcher, progress, host::current())
        .get()
        .map_err(|e| {
            format!("QEMU is not available for the VM tests: {e} (set {QEMU_DIR_VAR} to a QEMU build's directory)")
        })
}

/// A QMP socket path in a short, non-ASCII directory under the temp directory, and its lock file; both are deleted on
/// drop.
///
/// The directory is the shared `<temp>/<UNICODE_DIR>`: a socket path is limited to 107 bytes, which the per-test
/// directories of `unicode_home` exceed. Socket names carry a random instance id, so tests do not collide, and the
/// shared directory is never removed, so no test can find it gone.
pub struct Sock {
    pub endpoint: QmpEndpoint,
}

/// The shared directory control sockets go in during tests (created if need be): `<temp>/<UNICODE_DIR>`, or, where the
/// temporary directory is too long for a socket path (macOS), `/tmp/<UNICODE_DIR>`.
pub fn socket_dir() -> PathBuf {
    let fits = |dir: &Path| QmpEndpoint::for_instance(dir, &InstanceId::random()).is_ok();
    let dir = [std::env::temp_dir(), PathBuf::from("/tmp")]
        .into_iter()
        .map(|base| base.join(UNICODE_DIR))
        .find(|dir| fits(dir))
        .unwrap_or_else(|| panic!("no short directory for a control socket under the temporary directory"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

impl Sock {
    pub fn new() -> Sock {
        Sock {
            endpoint: QmpEndpoint::for_instance(&socket_dir(), &InstanceId::random()).unwrap(),
        }
    }
}

impl Drop for Sock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.endpoint.path());
        let _ = std::fs::remove_file(self.endpoint.lock_path());
    }
}

/// A QEMU with no machine (`-machine none`), frozen, listening for QMP, with one user-mode forward on port 0. No guest
/// and no accelerator: it needs only the sidecar. Killed on drop.
pub struct BareQemu {
    child: Child,
    pub sock: Sock,
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
                sock.endpoint.path().display()
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
        BareQemu { child, sock }
    }

    /// Whether the process is still running.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
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

/// A `LazyInstall` over the QEMU whose executables are in `bin`.
pub fn lazy_at(bin: &Path) -> Arc<LazyInstall> {
    let dirs = Dirs::under(&std::env::temp_dir());
    let progress = Arc::new(StderrProgress);
    let transport = HttpTransport::new().expect("an HTTP client");
    let fetcher = Arc::new(Fetcher::new(dirs.downloads(), Arc::new(transport), progress.clone()));
    let config = Config {
        qemu_path: Some(bin.to_path_buf()),
    };
    Arc::new(LazyInstall::new(dirs, config, fetcher, progress, host::current()))
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
