//! The sweep of the runtime directory: the files of machines that are dead.
//!
//! The directory is shared by every devvm setup of the user (an agent with its own `HOME`, a second test binary,
//! `Dirs::under`), and a socket's name carries no instance, so nothing here can tell whose a file is and nothing tries.
//! What tells a dead machine's file from a live one's is the hold on its claim file (`<name>.sock.hold`): QEMU holds it
//! for as long as it runs, so whoever can take it knows nobody is running, whichever setup started the machine.

use std::collections::BTreeSet;
use std::path::Path;

use super::backend::QemuBackend;
use super::qmp::QmpEndpoint;
use crate::backend::remove_held;
use crate::host::{DELETING_SUFFIX, HoldOutcome};
use crate::store::lock::{FileLock, Mode};

/// The 16 lowercase hex digits that name a launch, if `file_name` is one of its three files.
fn launch_of(file_name: &str) -> Option<&str> {
    let stem = [".sock.hold", ".sock.lock", ".sock"]
        .iter()
        .find_map(|suffix| file_name.strip_suffix(suffix))?;
    let hex = stem.len() == 16 && stem.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    hex.then_some(stem)
}

/// Whether `file_name` is a private name of a claim file: `<claim file>.new` while it is made (`Host::hold_new`) and
/// `<claim file>.del` while it is deleted (`Hold::delete`).
fn is_private_claim(file_name: &str) -> bool {
    [".new", DELETING_SUFFIX].iter().any(|suffix| {
        file_name
            .strip_suffix(suffix)
            .is_some_and(|claim| claim.ends_with(".sock.hold") && launch_of(claim).is_some())
    })
}

impl QemuBackend {
    /// Deletes the files of every machine in the runtime directory that is dead.
    ///
    /// Nothing is made and nothing is waited for. Without a directory, or one without its `runtime.lock`, there is
    /// nothing to do. A directory that a launch would refuse (`Host::accept_runtime_dir`: a link, a file, another
    /// account's) is warned about and left alone. The lock is tried exclusive: a launch holds it shared while it draws and claims a name, and
    /// another sweep may be running. Either way the pass ends and the next reap tries again.
    ///
    /// For each launch (names of the shape `<16 hex>.sock`, `.sock.lock` and `.sock.hold`, files and no others):
    /// - its claim file is held: the machine is alive, nothing is done;
    /// - its claim file is free: the machine is dead, and the socket, the lock file and the claim file go, in that order;
    /// - there is no claim file, no socket, and a lock file: a session that came late made it, and it goes;
    /// - there is no claim file but a socket: nobody can tell, and it stays.
    ///
    /// A claim file's private names (`<claim file>.new`, `<claim file>.del`) go too: only a launch that died while
    /// making the file, or a delete that died, leaves one.
    pub(super) fn sweep_run_dir(&self, warn: &mut dyn FnMut(String)) {
        let run = self.run_dir();
        match self.host().accept_runtime_dir(run) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                warn(e.to_string());
                return;
            }
        }
        let mut directory = match FileLock::open_existing(&run.join("runtime.lock")) {
            Ok(Some(lock)) => lock,
            Ok(None) => return,
            Err(e) => {
                warn(e.message().to_owned());
                return;
            }
        };
        match directory.try_lock(Mode::Exclusive) {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                warn(e.message().to_owned());
                return;
            }
        }
        let listing = match std::fs::read_dir(run) {
            Ok(listing) => listing,
            Err(e) => {
                warn(format!("cannot list {}: {e}", run.display()));
                return;
            }
        };
        let (mut launches, mut private_claims) = (BTreeSet::new(), Vec::new());
        for entry in listing {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    warn(format!("cannot list {}: {e}", run.display()));
                    return;
                }
            };
            let is_file = entry.file_type().is_ok_and(|kind| !kind.is_dir() && !kind.is_symlink());
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|_| is_file) else {
                continue;
            };
            if let Some(launch) = launch_of(name) {
                launches.insert(launch.to_owned());
            } else if is_private_claim(name) {
                private_claims.push(entry.path());
            }
        }
        // No launch is between making a claim file and holding it while this sweep has the directory's lock, so what
        // is left under `.new` was left by a launch that died there. A `.del` is left by a delete that died, or is
        // being deleted now (the lock does not keep deletes out): the host leaves that one to its deleter.
        for leftover in private_claims {
            let removed = if leftover
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(DELETING_SUFFIX))
            {
                self.host().remove_deleting_name(&leftover)
            } else {
                std::fs::remove_file(&leftover)
            };
            if let Err(e) = removed
                && e.kind() != std::io::ErrorKind::NotFound
            {
                warn(format!("cannot delete {}: {e}", leftover.display()));
            }
        }
        for launch in launches {
            self.sweep_launch(run, &launch, warn);
        }
    }

    fn sweep_launch(&self, run: &Path, launch: &str, warn: &mut dyn FnMut(String)) {
        let endpoint = QmpEndpoint::from_path(run.join(format!("{launch}.sock")));
        let (socket, lock, claim) = (
            endpoint.path().to_path_buf(),
            endpoint.lock_path(),
            endpoint.hold_path(),
        );
        match self.host().hold_existing(&claim) {
            Ok(HoldOutcome::Taken(hold)) => {
                remove_held(self.host(), hold, &claim, &[socket, lock, claim.clone()], warn)
            }
            Ok(HoldOutcome::Busy) => {}
            Ok(HoldOutcome::Absent) => {
                if lock.exists()
                    && !socket.exists()
                    && let Err(e) = std::fs::remove_file(&lock)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    warn(format!("cannot delete {}: {e}", lock.display()));
                }
            }
            Err(e) => warn(format!("cannot hold {}: {e}", claim.display())),
        }
    }
}

#[cfg(test)]
#[path = "sweep_tests.rs"]
mod sweep_tests;

#[cfg(all(test, unix))]
#[path = "sweep_unix_tests.rs"]
mod sweep_unix_tests;

#[cfg(all(test, windows))]
#[path = "sweep_windows_tests.rs"]
mod sweep_windows_tests;
