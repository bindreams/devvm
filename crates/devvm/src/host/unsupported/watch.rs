//! Waiting for a process on a host whose AF_UNIX streams end when the peer's process exits.

use cosca::Process;
use cosca::identity::ProcessId;

use crate::Result;
use crate::backend::qemu::uds::UnixStream;
use crate::contain::cosca_error;
use crate::host::watch::{Link, ProcessWatch, RETRY};

#[cfg(test)]
#[path = "watch_tests.rs"]
mod watch_tests;

pub(super) struct UnixWatch {
    id: ProcessId,
    /// `None`: no process has this identity any more, so it has exited.
    process: Option<Process>,
}

impl UnixWatch {
    pub(super) fn new(id: ProcessId) -> Result<UnixWatch> {
        let process = Process::open(id).map_err(|e| {
            cosca_error(
                &format!("cannot open the VM process {} to wait for its exit", id.pid()),
                e,
            )
        })?;
        Ok(UnixWatch { id, process })
    }

    fn exited_within(&self, limit: std::time::Duration) -> Result<bool> {
        let Some(process) = &self.process else {
            return Ok(true);
        };
        process
            .wait_timeout(limit)
            .map_err(|e| cosca_error(&format!("cannot wait for the VM process {}", self.id.pid()), e))
    }
}

impl ProcessWatch for UnixWatch {
    fn has_exited(&self) -> Result<bool> {
        self.exited_within(std::time::Duration::ZERO)
    }

    fn pause(&self) -> Result<bool> {
        self.exited_within(RETRY)
    }

    /// The OS ends the stream when the peer's process exits, so a plain stream is a link.
    fn link(&self, stream: UnixStream) -> std::io::Result<Box<dyn Link>> {
        Ok(Box::new(stream))
    }
}
