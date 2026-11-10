//! Where the VM tests keep their files: the XDG roots below a directory named `UNICODE_DIR` (a space, non-ASCII
//! characters, a surrogate pair), so each path devvm builds in these tests carries them.
//!
//! Both test binaries include this file by `#[path]` (the lib's `test_support.rs` and `devvm-cli/tests/vm_cli.rs`),
//! so there is one base picker. The including module names `UNICODE_DIR` and the crate root has `test_groups`.
//!
//! The directory is in `C:\Users` where the `unicode_base` group is on (the literal location of a user's directories; a
//! hosted runner is elevated and can make it), and in the temporary directory where it is off.

#![allow(dead_code, reason = "each including test binary uses part of it")]

use std::path::{Path, PathBuf};

use super::UNICODE_DIR;
use crate::test_groups::unicode_base;

/// The directory the roots go below where `unicode_base` is on.
pub(crate) const USERS: &str = r"C:\Users";

/// The name of the cache directory every test process shares, in the directory named `UNICODE_DIR`.
const SHARED_CACHE: &str = "devvm-test-cache";

/// Where the directory named `UNICODE_DIR` is: `C:\Users` where `unicode_base` is on, else the temporary directory.
/// Where the group is on and the host is not Windows the error says so: `C:\Users` there is a relative path.
pub(crate) fn base_dir() -> Result<PathBuf, String> {
    match unicode_base::enabled() {
        Ok(()) if cfg!(windows) => Ok(PathBuf::from(USERS)),
        Ok(()) => Err(format!(
            r"the base is C:\Users, which only Windows has; {}",
            unicode_base::OPT_OUT
        )),
        Err(_) => Ok(std::env::temp_dir()),
    }
}

/// Shorter fallbacks for [`socket_dir`], each with a space and non-ASCII characters like `UNICODE_DIR`: the first also
/// has a character outside the Basic Multilingual Plane (a UTF-16 surrogate pair).
const SHORT_SOCKET_DIRS: [&str; 2] = ["\u{e9} \u{4e16}\u{1f600}", "\u{e9} \u{4e16}"];

/// The directory control sockets go in during tests, if any fits (`fits` says whether a socket path in a directory is
/// within the host's limit): the first of `<temp>/<UNICODE_DIR>`, the shorter `<temp>/<SHORT_SOCKET_DIRS>` (a long
/// Windows user name), then the temporary directory itself (a temporary directory that leaves room for none of them).
/// Nothing is created. The roots' own directories are too long for a socket path, so a runtime directory is this one.
pub(crate) fn socket_dir(fits: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let temp = std::env::temp_dir();
    std::iter::once(temp.join(UNICODE_DIR))
        .chain(SHORT_SOCKET_DIRS.iter().map(|short| temp.join(short)))
        .chain(std::iter::once(temp.clone()))
        .find(|dir| fits(dir))
}

/// Whether `path` has a space and a character outside ASCII, as a directory named `UNICODE_DIR` or a fallback of
/// [`socket_dir`] does.
pub(crate) fn has_space_and_non_ascii(path: &Path) -> bool {
    let path = path.to_string_lossy();
    path.contains(' ') && !path.is_ascii()
}

/// A process's XDG directories: `config`, `data` and `state` are new, below `top` (`<base>\<UNICODE_DIR>\<run id>`), and
/// removed when this is dropped; `cache` is `<base>\<UNICODE_DIR>\devvm-test-cache`, which every process shares and
/// nothing removes, so the QEMU build and the Ubuntu image are fetched once per machine.
pub(crate) struct XdgRoots {
    _top: skuld::TempDir,
    /// `<base>\<UNICODE_DIR>\<run id>`.
    pub top: PathBuf,
    /// `<base>\<UNICODE_DIR>`.
    pub user: PathBuf,
    /// The environment variables that name the roots: the four `XDG_*_HOME` ones, and `XDG_RUNTIME_DIR` when `new` was
    /// given a runtime directory.
    pub vars: Vec<(String, PathBuf)>,
}

impl XdgRoots {
    /// Makes the roots. `run` is `XDG_RUNTIME_DIR`: a short directory, because a control socket path is limited;
    /// `None` leaves it unnamed. A base that cannot be made is an error naming it and how to turn the group off.
    pub(crate) fn new(run: Option<&Path>) -> Result<XdgRoots, String> {
        let user = base_dir()?.join(UNICODE_DIR);
        let make = |dir: &Path| {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot make {}: {e}; {}", dir.display(), unicode_base::OPT_OUT))
        };
        make(&user)?;
        let top = skuld::TempDir::new_in(&user).map_err(|e| {
            format!(
                "cannot make a run directory in {}: {e}; {}",
                user.display(),
                unicode_base::OPT_OUT
            )
        })?;
        let cache = user.join(SHARED_CACHE);
        let vars: Vec<(String, PathBuf)> = ["config", "data", "state"]
            .iter()
            .map(|name| (*name, top.path().join(name)))
            .chain([("cache", cache)])
            .map(|(name, dir)| (format!("XDG_{}_HOME", name.to_uppercase()), dir))
            .collect();
        for (_, dir) in &vars {
            make(dir)?;
        }
        let vars = vars
            .into_iter()
            .chain(run.map(|run| ("XDG_RUNTIME_DIR".to_owned(), run.to_path_buf())))
            .collect();
        Ok(XdgRoots {
            top: top.path().to_path_buf(),
            _top: top,
            user,
            vars,
        })
    }

    /// The root `XDG_<name>_HOME` names (`name` in upper case), or `XDG_RUNTIME_DIR`.
    pub(crate) fn var(&self, name: &str) -> Option<&Path> {
        self.vars
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, dir)| dir.as_path())
    }
}
