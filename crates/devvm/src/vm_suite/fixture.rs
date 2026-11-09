//! Where the VM tests keep their files, and the engine over them.
//!
//! Every root is below a directory named [`UNICODE_DIR`] (a space, non-ASCII characters, a surrogate pair), so each path
//! devvm builds in these tests carries them. The directory is in `C:\Users` where the `unicode_base` group is on (the
//! literal location of a user's directories; a hosted runner is elevated and can make it), and in the temporary
//! directory where it is off.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::Dirs;
use crate::backend::qemu::qemu_parts;
use crate::backend::qemu::sidecar::QemuInstall;
use crate::config::Config;
use crate::engine::Engine;
use crate::engine::engine_tests::engine_with;
use crate::exit::StderrProgress;
use crate::host;
use crate::random;
use crate::store::fetch::{Fetcher, HttpTransport};
use crate::test_groups::{Group, unicode_base};
use crate::test_support::fixtures::UNICODE_DIR;
use crate::test_support::qemu::{qemu_sidecar, socket_dir, write_config};

/// The directory the roots go below where `unicode_base` is on.
pub(super) const USERS: &str = r"C:\Users";

/// The name of the cache directory every test process shares, in the directory named [`UNICODE_DIR`].
const SHARED_CACHE: &str = "devvm-test-cache";

/// Where the directory named [`UNICODE_DIR`] is: `C:\Users` where `unicode_base` is on, else the temporary directory.
fn base_dir() -> PathBuf {
    match unicode_base::enabled() {
        Ok(()) => PathBuf::from(USERS),
        Err(_) => std::env::temp_dir(),
    }
}

/// A process's directories: `config`, `data` and `state` are new, below `<base>\<UNICODE_DIR>\<run id>`, and removed
/// when this is dropped; `cache` is `<base>\<UNICODE_DIR>\devvm-test-cache`, which every process shares and nothing
/// removes, so the QEMU build and the Ubuntu image are fetched once per machine.
pub(super) struct Roots {
    top: PathBuf,
    pub dirs: Dirs,
}

impl Roots {
    /// Makes the roots. `run` is `XDG_RUNTIME_DIR`: a short directory, because a control socket path is limited
    /// ([`socket_dir`]); `None` leaves the runtime directory unnamed. A base that cannot be made is an error naming it
    /// and how to turn the group off; it never falls back to another.
    pub(super) fn new(run: Option<&Path>) -> Result<Roots, String> {
        let user = base_dir().join(UNICODE_DIR);
        let top = user.join(hex::encode(random::bytes::<4>()));
        let cache = user.join(SHARED_CACHE);
        let vars: Vec<(String, PathBuf)> = ["config", "data", "state"]
            .iter()
            .map(|name| (*name, top.join(name)))
            .chain([("cache", cache)])
            .map(|(name, dir)| (format!("XDG_{}_HOME", name.to_uppercase()), dir))
            .chain(run.map(|run| ("XDG_RUNTIME_DIR".to_owned(), run.to_path_buf())))
            .collect();
        for (name, dir) in vars.iter().filter(|(name, _)| name != "XDG_RUNTIME_DIR") {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot make {name} {}: {e}; {}", dir.display(), unicode_base::OPT_OUT))?;
        }
        let var = |name: &str| -> Option<OsString> {
            vars.iter()
                .find(|(key, _)| key == name)
                .map(|(_, dir)| dir.clone().into_os_string())
        };
        let dirs = Dirs::from_lookup(Some(top.clone()), &var, &|| {
            Err(crate::Error::new(
                crate::ErrorKind::Config,
                "these roots name no runtime directory",
            ))
        })
        .map_err(|e| format!("cannot resolve the roots: {e}"))?;
        Ok(Roots { top, dirs })
    }
}

impl Drop for Roots {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.top);
    }
}

/// An engine over `dirs` on the QEMU of `install`, used in place, with the real downloader.
pub(super) fn engine_in(dirs: &Dirs, install: &QemuInstall) -> Arc<Engine> {
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
    engine_with(
        dirs.clone(),
        host::current(),
        Some(parts.backend),
        parts.guests,
        progress,
    )
}

/// The engine the tests of a guest share, with the roots it reads: its layer store holds the Ubuntu base once per test
/// process.
pub(super) struct VmEngine {
    pub engine: Arc<Engine>,
    _roots: Roots,
}

#[skuld::fixture(scope = process)]
pub(super) fn vm_engine(#[fixture(qemu_sidecar)] install: &Arc<QemuInstall>) -> Result<VmEngine, String> {
    let roots = Roots::new(Some(&socket_dir()))?;
    let engine = engine_in(&roots.dirs, install);
    Ok(VmEngine { engine, _roots: roots })
}

// No VM ===============================================================================================================

/// What a user's directories are: the four XDG variables name directories below the user's, and `devvm`'s own are in
/// them. Where `unicode_base` is on the base is the literal `C:\Users`, so a drop of the variable from a workflow fails
/// here, not by quietly testing the temporary directory.
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
