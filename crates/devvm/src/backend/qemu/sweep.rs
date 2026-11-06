//! The sweep of the runtime directory: the files of machines that are dead.
//!
//! The directory is shared by every devvm setup of the user (an agent with its own `HOME`, a second test binary,
//! `Dirs::under`), and a socket's name carries no instance, so nothing here can tell whose a file is and nothing tries.
//! What tells a dead machine's file from a live one's is the hold on its claim file (`<name>.sock.hold`): QEMU holds it
//! for as long as it runs, so whoever can take it knows nobody is running, whichever setup started the machine.

use std::collections::BTreeSet;
use std::path::Path;

use super::backend::QemuBackend;
use super::qmp::{LaunchFileKind, QmpEndpoint};
use crate::backend::remove_held;
use crate::host::{HoldOutcome, claim_of_deleting_name, claim_of_new_name, is_absent, symlink_metadata_if_present};
use crate::store::lock::{FileLock, Mode};

/// Which private name of a claim file a file is.
enum PrivateClaimKind {
    /// `<claim file>.new`, while it is made (`Host::hold_new`).
    New,
    /// `<claim file>.<16 hex>.del`, while it is deleted (`Hold::delete`).
    Deleting,
}

/// The private name of a claim file that `file_name` is, if it is one.
fn private_claim_kind(file_name: &str) -> Option<PrivateClaimKind> {
    let is_of_a_launch = |name: &str| matches!(QmpEndpoint::launch_file(name), Some((_, LaunchFileKind::Claim)));
    if let Some(claim) = claim_of_new_name(file_name)
        && is_of_a_launch(claim)
    {
        Some(PrivateClaimKind::New)
    } else if let Some(claim) = claim_of_deleting_name(file_name)
        && is_of_a_launch(claim)
    {
        Some(PrivateClaimKind::Deleting)
    } else {
        None
    }
}

impl QemuBackend {
    /// Deletes the files of every machine in the runtime directory that is dead.
    ///
    /// Nothing is made and nothing is waited for. A host that cannot name the directory is warned about, and so is a
    /// directory that a launch would refuse (`Host::accept_runtime_dir`: a link, a file, another account's), which is
    /// left alone. The lock is tried exclusive: a launch holds it shared while it draws and claims a name, and another
    /// sweep may be running. Either way the pass ends and the next reap tries again.
    ///
    /// For each launch (names of the shape `<16 hex>.sock`, `.sock.lock` and `.sock.hold`, files and no others):
    /// - its claim file is held: the machine is alive, nothing is done;
    /// - its claim file is free: the machine is dead or dying, and the socket, the lock file and the claim file go, in
    ///   that order;
    /// - there is no claim file, no socket, and a lock file: a session that came late made it, and it goes;
    /// - there is no claim file but a socket: no launch leaves a socket without its claim (the claim is made first and
    ///   deleted last, after the socket and lock are gone), so someone deleted the claim by hand, and the socket stays.
    ///
    /// A claim file's private names (`<claim file>.new`, `<claim file>.<16 hex>.del`) go too: only a launch that died
    /// while making the file, or a delete that died, leaves one.
    pub(super) fn sweep_run_dir(&self, warn: &mut dyn FnMut(String)) {
        let run = match self.dirs.run() {
            Ok(run) => run,
            Err(e) => {
                warn(e.message().to_owned());
                return;
            }
        };
        match self.host().accept_runtime_dir(run) {
            Ok(()) => {}
            Err(e) if is_absent(&e) => return,
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
            let is_file = match entry.file_type() {
                Ok(kind) => !kind.is_dir() && !kind.is_symlink(),
                Err(e) if is_absent(&e) => continue,
                Err(e) => {
                    warn(format!("cannot look at {}: {e}", entry.path().display()));
                    continue;
                }
            };
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|_| is_file) else {
                continue;
            };
            if let Some((launch, _)) = QmpEndpoint::launch_file(name) {
                launches.insert(launch.to_owned());
            } else if let Some(kind) = private_claim_kind(name) {
                private_claims.push((entry.path(), kind));
            }
        }
        // No launch is between making a claim file and holding it while this sweep has the directory's lock, so what
        // is left under `.new` was left by a launch that died there. A `.del` is left by a delete that died, or is
        // being deleted now (the lock does not keep deletes out): the host leaves that one to its deleter.
        for (leftover, kind) in private_claims {
            let removed = match kind {
                PrivateClaimKind::Deleting => self.host().remove_deleting_name(&leftover),
                PrivateClaimKind::New => std::fs::remove_file(&leftover),
            };
            if let Err(e) = removed
                && !is_absent(&e)
            {
                warn(format!("cannot delete {}: {e}", leftover.display()));
            }
        }
        for launch in launches {
            self.sweep_launch(run, &launch, warn);
        }
    }

    fn sweep_launch(&self, run: &Path, launch: &str, warn: &mut dyn FnMut(String)) {
        let endpoint = QmpEndpoint::of_launch(run, launch);
        let claim = endpoint.hold_path();
        match self.host().hold_existing(&claim) {
            Ok(HoldOutcome::Taken(hold)) => remove_held(self.host(), hold, &claim, &endpoint.launch_files(), warn),
            // Another deleter or an inheritor of the hold owns the cleanup.
            Ok(HoldOutcome::Busy) => {}
            Ok(HoldOutcome::Absent) => {
                // A late session can make a lock after the claim is gone, and a socket cannot appear after that: a
                // lock whose socket is gone is left over. A lookup that fails says nothing about the file, so
                // nothing is deleted.
                let (socket, lock) = (endpoint.path(), endpoint.lock_path());
                match symlink_metadata_if_present(&lock) {
                    Ok(None) => {}
                    Err(e) => warn(format!("cannot look at {}: {e}", lock.display())),
                    Ok(Some(_)) => match symlink_metadata_if_present(socket) {
                        Ok(None) => {
                            if let Err(e) = std::fs::remove_file(&lock)
                                && !is_absent(&e)
                            {
                                warn(format!("cannot delete {}: {e}", lock.display()));
                            }
                        }
                        Ok(Some(_)) => {}
                        Err(e) => warn(format!("cannot look at {}: {e}", socket.display())),
                    },
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
