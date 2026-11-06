//! XDG-style directory resolution, identical on every OS.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "dirs_tests.rs"]
mod dirs_tests;

/// devvm's five per-user directories. The four XDG ones end in `devvm`; `run` ends in `devvm` below
/// `XDG_RUNTIME_DIR` and is `devvm-run-<...>` where the host names it (`Host::run_dir_default`).
///
/// They are created lazily by their users. `run` fails only when asked for: a command that never touches a control
/// socket does not depend on the host being able to name it.
#[derive(Clone, Debug)]
pub struct Dirs {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub state: PathBuf,
    /// The runtime directory, or why the host could not name it.
    run: Result<PathBuf>,
}

impl Dirs {
    /// Resolves from the process environment and the user's home directory.
    pub fn from_env() -> Result<Dirs> {
        let host = crate::host::current();
        Dirs::from_lookup(home::home_dir(), &|name| std::env::var_os(name), &|| {
            host.run_dir_default(&|name| std::env::var_os(name))
        })
    }

    /// Resolves with `var` standing in for the environment. An `XDG_*` value that is empty or not an absolute path is
    /// ignored, as the XDG specification says. `run_default` gives the runtime directory when `XDG_RUNTIME_DIR` is
    /// not usable; it is not called otherwise, and its failure is [`Dirs::run`]'s, not this function's.
    pub fn from_lookup(
        home: Option<PathBuf>,
        var: &dyn Fn(&str) -> Option<OsString>,
        run_default: &dyn Fn() -> Result<PathBuf>,
    ) -> Result<Dirs> {
        let resolve = |name: &str, default: &[&str]| -> Result<PathBuf> {
            let explicit = var(name).map(PathBuf::from).filter(|path| path.is_absolute());
            let base = match explicit {
                Some(path) => path,
                None => {
                    let home = home.as_deref().ok_or_else(|| {
                        Error::new(
                            ErrorKind::Config,
                            format!(
                                "cannot locate a directory: {name} is unset and neither HOME nor USERPROFILE is known"
                            ),
                        )
                    })?;
                    default.iter().fold(home.to_path_buf(), |path, part| path.join(part))
                }
            };
            Ok(base.join("devvm"))
        };
        let run = match var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
        {
            Some(path) => Ok(path.join("devvm")),
            None => run_default(),
        };
        Ok(Dirs {
            run,
            config: resolve("XDG_CONFIG_HOME", &[".config"])?,
            data: resolve("XDG_DATA_HOME", &[".local", "share"])?,
            cache: resolve("XDG_CACHE_HOME", &[".cache"])?,
            state: resolve("XDG_STATE_HOME", &[".local", "state"])?,
        })
    }

    /// All five directories below `root`, for tests and fixtures.
    pub fn under(root: &Path) -> Dirs {
        Dirs {
            config: root.join("config").join("devvm"),
            data: root.join("data").join("devvm"),
            cache: root.join("cache").join("devvm"),
            state: root.join("state").join("devvm"),
            run: Ok(root.join("run").join("devvm")),
        }
    }

    /// Runtime state of running machines (their control sockets): short, and the user's alone.
    pub fn run(&self) -> Result<&Path> {
        self.run.as_deref().map_err(Error::clone)
    }

    pub fn layers(&self) -> PathBuf {
        self.data.join("layers")
    }

    pub fn downloads(&self) -> PathBuf {
        self.cache.join("downloads")
    }

    /// Where unparseable records are moved instead of being deleted.
    pub fn quarantine(&self) -> PathBuf {
        self.data.join("quarantine")
    }

    /// One directory per instance, named by the instance.
    pub fn instances(&self) -> PathBuf {
        self.state.join("instances")
    }

    /// Owner and op lock files, keyed by instance id and kept outside the instance directories.
    pub fn instance_locks(&self) -> PathBuf {
        self.state.join("locks")
    }

    /// Where the QEMU sidecars are extracted.
    pub fn qemu(&self) -> PathBuf {
        self.cache.join("qemu")
    }

    /// The `ssh_config` and `known_hosts` files devvm writes for the system `ssh`.
    pub fn ssh(&self) -> PathBuf {
        self.state.join("ssh")
    }

    /// Where unreadable instance directories are moved instead of being deleted.
    pub fn instance_quarantine(&self) -> PathBuf {
        self.state.join("quarantine")
    }
}
