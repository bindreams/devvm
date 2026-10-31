//! Waiting for a process on a host whose AF_UNIX streams end when the peer's process exits.

use cosca::Process;
use cosca::identity::ProcessId;

use crate::backend::qemu::uds::UnixStream;
use crate::host::watch::{Link, ProcessWatch, RETRY};
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "watch_tests.rs"]
mod watch_tests;

pub(super) struct UnixWatch {
    process: Process,
}

impl UnixWatch {
    pub(super) fn new(id: ProcessId) -> UnixWatch {
        UnixWatch {
            process: Process::from_id(id),
        }
    }

    fn exited_within(&self, limit: std::time::Duration) -> Result<bool> {
        self.process.wait_timeout(limit).map_err(|e| {
            Error::new(
                ErrorKind::Io,
                format!("cannot wait for the VM process {}: {e}", self.process.id().pid()),
            )
        })
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
