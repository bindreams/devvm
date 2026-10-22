//! XDG-style directory resolution, identical on every OS.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "dirs_tests.rs"]
mod dirs_tests;

/// devvm's four per-user directories; each path ends in `devvm`.
///
/// They are created lazily by their users.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dirs {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub state: PathBuf,
}

impl Dirs {
    /// Resolves from the process environment and the user's home directory.
    pub fn from_env() -> Result<Dirs> {
        Dirs::from_lookup(home::home_dir(), &|name| std::env::var_os(name))
    }

    /// Resolves with `var` standing in for the environment. An `XDG_*` value that is empty or not an absolute path is
    /// ignored, as the XDG specification says.
    pub fn from_lookup(home: Option<PathBuf>, var: &dyn Fn(&str) -> Option<OsString>) -> Result<Dirs> {
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
        Ok(Dirs {
            config: resolve("XDG_CONFIG_HOME", &[".config"])?,
            data: resolve("XDG_DATA_HOME", &[".local", "share"])?,
            cache: resolve("XDG_CACHE_HOME", &[".cache"])?,
            state: resolve("XDG_STATE_HOME", &[".local", "state"])?,
        })
    }

    /// All four directories below `root`, for tests and fixtures.
    pub fn under(root: &Path) -> Dirs {
        Dirs {
            config: root.join("config").join("devvm"),
            data: root.join("data").join("devvm"),
            cache: root.join("cache").join("devvm"),
            state: root.join("state").join("devvm"),
        }
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
}
