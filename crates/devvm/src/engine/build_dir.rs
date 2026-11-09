//! The directory a build keeps its `copy` snapshots in, and the sweep of those of builds that died.
//!
//! A build names itself by `<hex>` and takes the lock `build-<hex>.lock` (`Locks::acquire_build_new_with`) before it
//! makes `<state>/builds/<hex>`, so a sweep that sees the directory sees the lock file of a live build, or of none (a
//! builder that died or finished unlinked it after the directory).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::Engine;
use crate::host::{remove_tree, symlink_metadata_if_present};
use crate::instance::locks::Probe;
use crate::store::lock::FileLock;
use crate::{Error, ErrorKind, Result, random};

#[cfg(test)]
#[path = "build_dir_tests.rs"]
mod build_dir_tests;

/// One build's directory, with the lock that names it held for as long as this lives. Dropping it (the build returned,
/// failed or unwound) removes the directory and then the lock file, still holding the lock. A removal that fails is a
/// warning: the lock file stays, and the next [`sweep`] retries. A build that is killed leaves both.
pub(super) struct BuildDir<'a> {
    engine: &'a Engine,
    hex: String,
    path: PathBuf,
    _lock: FileLock,
}

impl<'a> BuildDir<'a> {
    pub(super) fn create(engine: &'a Engine) -> Result<BuildDir<'a>> {
        BuildDir::create_with(engine, &mut || {}, &mut || {})
    }

    /// `create`, with the seams of `Locks::acquire_build_new_with`.
    pub(super) fn create_with(
        engine: &'a Engine,
        between: &mut dyn FnMut(),
        before_look: &mut dyn FnMut(),
    ) -> Result<BuildDir<'a>> {
        let hex = hex::encode(random::bytes::<16>());
        let lock = engine
            .registry()
            .locks()
            .acquire_build_new_with(&hex, between, before_look)?;
        let build = BuildDir {
            engine,
            path: engine.dirs.builds().join(&hex),
            hex,
            _lock: lock,
        };
        // From here the guard removes the lock file, whether or not the directory is made.
        std::fs::create_dir_all(&build.path).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot create the build directory {}", build.path.display()),
                e,
            )
        })?;
        Ok(build)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BuildDir<'_> {
    fn drop(&mut self) {
        let progress = self.engine.progress();
        if let Err(e) = remove_tree(&self.path) {
            progress.message(&format!("cannot remove {}: {e}", self.path.display()));
            return;
        }
        if let Err(e) = self.engine.registry().locks().remove_build(&self.hex) {
            progress.message(e.message());
        }
    }
}

/// Removes the directory and the lock file of every build that is no longer running: the lock is free, or its file is
/// not there. A build whose lock is held is skipped, and so is a name that is not a build's. A problem is a warning on
/// the progress channel that the next sweep retries, and nothing the sweep could not look at is deleted. A directory
/// with no lock file is removed under `dead.lock`, which it only tries, so another deleter's tree is left to the next
/// sweep. It makes nothing but that `dead.lock` (and the locks directory), whenever a build it found has no lock file:
/// a directory with none, or a build that finished and unlinked its lock after the sweep listed it.
pub(super) fn sweep(engine: &Engine) {
    let locks = engine.registry().locks();
    let progress = engine.progress();
    let mut builds = BTreeSet::new();
    match locks.build_locks() {
        Ok(found) => builds.extend(found),
        Err(e) => progress.message(e.message()),
    }
    list_directories(engine, &mut builds);
    for hex in builds {
        match locks.try_build(&hex) {
            Ok(Probe::Held) => {}
            Ok(Probe::Free(_lock)) => {
                if remove_directory(engine, &hex)
                    && let Err(e) = locks.remove_build(&hex)
                {
                    progress.message(e.message());
                }
            }
            // No lock file to serialize on: two sweeps removing one tree make each other fail on Windows, so the one that
            // does not hold `dead.lock` leaves it to the next sweep.
            Ok(Probe::Gone) => match locks.deleting(|| remove_directory(engine, &hex)) {
                Ok(_) => {}
                Err(e) => progress.message(&format!("cannot delete build {hex}: {}", e.message())),
            },
            Err(e) => progress.message(&format!("cannot look at the lock of build {hex}: {}", e.message())),
        }
    }
}

/// Adds the name of every directory of `<state>/builds` that is named like a build's.
fn list_directories(engine: &Engine, into: &mut BTreeSet<String>) {
    let progress = engine.progress();
    let builds = engine.dirs.builds();
    match symlink_metadata_if_present(&builds) {
        Ok(Some(_)) => {}
        Ok(None) => return,
        Err(e) => {
            progress.message(&format!("cannot look at {}: {e}", builds.display()));
            return;
        }
    }
    let entries = match std::fs::read_dir(&builds) {
        Ok(entries) => entries,
        Err(e) => {
            progress.message(&format!("cannot list {}: {e}", builds.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                progress.message(&format!("cannot list {}: {e}", builds.display()));
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| random::is_hex(name, 16)) else {
            continue;
        };
        match is_directory(&entry.path()) {
            Ok(true) => {
                into.insert(name.to_owned());
            }
            Ok(false) => {}
            Err(e) => progress.message(&format!("cannot tell what {} is: {e}", entry.path().display())),
        }
    }
}

/// Whether `path` is a directory, not a link to one. A path that is gone is not: a build that finished since the
/// listing took its directory with it.
fn is_directory(path: &Path) -> std::io::Result<bool> {
    Ok(symlink_metadata_if_present(path)?.is_some_and(|meta| meta.is_dir()))
}

/// Removes `<state>/builds/<hex>`; whether nothing is left of it. A failure is a warning.
fn remove_directory(engine: &Engine, hex: &str) -> bool {
    let path = engine.dirs.builds().join(hex);
    match remove_tree(&path) {
        Ok(()) => true,
        Err(e) => {
            engine
                .progress()
                .message(&format!("cannot remove {}: {e}", path.display()));
            false
        }
    }
}
