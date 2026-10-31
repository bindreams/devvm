//! A flag with a condition variable.

use std::sync::{Arc, Condvar, Mutex};

use crate::backend::qemu::uds::UnixStream;
use crate::host::watch::RETRY;
use crate::host::{Link, ProcessWatch};

#[derive(Default)]
struct Flag {
    exited: Mutex<bool>,
    changed: Condvar,
}

impl ProcessWatch for Flag {
    fn has_exited(&self) -> crate::Result<bool> {
        Ok(*self.exited.lock().unwrap())
    }

    fn pause(&self) -> crate::Result<bool> {
        let exited = self.exited.lock().unwrap();
        let (exited, _) = self
            .changed
            .wait_timeout_while(exited, RETRY, |exited| !*exited)
            .unwrap();
        Ok(*exited)
    }

    fn link(&self, stream: UnixStream) -> std::io::Result<Box<dyn Link>> {
        Ok(Box::new(stream))
    }
}

pub struct ManualExit {
    flag: Arc<Flag>,
}

impl ManualExit {
    pub fn new() -> ManualExit {
        ManualExit { flag: Arc::default() }
    }

    /// The watch that reports the exit once `exit` has been called.
    pub fn watch(&self) -> Arc<dyn ProcessWatch> {
        self.flag.clone()
    }

    /// The process exits. Calling it again does nothing.
    pub fn exit(&self) {
        *self.flag.exited.lock().unwrap() = true;
        self.flag.changed.notify_all();
    }
}
