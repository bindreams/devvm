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
use crate::host::HoldOutcome;
use crate::store::lock::{FileLock, Mode};

/// The 16 lowercase hex digits that name a launch, if `file_name` is one of its three files.
fn launch_of(file_name: &str) -> Option<&str> {
    let stem = [".sock.hold", ".sock.lock", ".sock"]
        .iter()
        .find_map(|suffix| file_name.strip_suffix(suffix))?;
    let hex = stem.len() == 16 && stem.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    hex.then_some(stem)
}

impl QemuBackend {
    /// Deletes the files of every machine in the runtime directory that is dead.
    ///
    /// Nothing is made and nothing is waited for. Without a directory, or one without its `runtime.lock`, there is
    /// nothing to do. The lock is tried exclusive: a launch holds it shared while it draws and claims a name, and
    /// another sweep may be running. Either way the pass ends and the next reap tries again.
    ///
    /// For each launch (names of the shape `<16 hex>.sock`, `.sock.lock` and `.sock.hold`, files and no others):
    /// - its claim file is held: the machine is alive, nothing is done;
    /// - its claim file is free: the machine is dead, and the socket, the lock file and the claim file go, in that order;
    /// - there is no claim file, no socket, and a lock file: a session that came late made it, and it goes;
    /// - there is no claim file but a socket: nobody can tell, and it stays.
    pub(super) fn sweep_run_dir(&self, warn: &mut dyn FnMut(String)) {
        let run = self.run_dir();
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
        let mut launches = BTreeSet::new();
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
            if let (true, Some(launch)) = (is_file, name.to_str().and_then(launch_of)) {
                launches.insert(launch.to_owned());
            }
        }
        for launch in launches {
            self.sweep_launch(run, &launch, warn);
        }
    }

    fn sweep_launch(&self, run: &Path, launch: &str, warn: &mut dyn FnMut(String)) {
        let endpoint = QmpEndpoint::from_record(&run.join(format!("{launch}.sock")).display().to_string());
        let (socket, lock, claim) = (
            endpoint.path().to_path_buf(),
            endpoint.lock_path(),
            endpoint.hold_path(),
        );
        match self.host().hold_existing(&claim) {
            Ok(HoldOutcome::Taken(hold)) => remove_held(hold, &claim, &[socket, lock, claim.clone()], warn),
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
