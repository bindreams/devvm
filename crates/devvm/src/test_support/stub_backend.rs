//! A backend whose answers a test scripts: whether `attach` finds a machine, and whether stopping it works. It
//! launches nothing; it is for the code that finds and ends machines through the backend.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::backend::fake::FakeStorage;
use crate::backend::{Backend, Endpoint, Machine, MachineRecord, MachineSpec, Storage};
use crate::{Error, ErrorKind, Result};

/// What `attach` answers.
#[derive(Clone, Debug)]
pub enum Attach {
    /// A machine, whatever process the record names.
    Running,
    /// `NotFound`: no such machine runs.
    Gone,
    /// This error.
    Fails(ErrorKind),
}

pub struct StubBackend {
    attach: Mutex<Attach>,
    stop_fails: Mutex<bool>,
    calls: Arc<Mutex<Vec<String>>>,
    storage: FakeStorage,
}

impl StubBackend {
    pub fn new(attach: Attach) -> Arc<StubBackend> {
        Arc::new(StubBackend {
            attach: Mutex::new(attach),
            stop_fails: Mutex::new(false),
            calls: Arc::new(Mutex::new(Vec::new())),
            storage: FakeStorage,
        })
    }

    /// Changes what `attach` answers from now on.
    pub fn answer(&self, attach: Attach) {
        *self.attach.lock().unwrap() = attach;
    }

    /// From now on `shutdown` and `kill` of the machines found fail (and are still counted).
    pub fn fail_stops(&self) {
        *self.stop_fails.lock().unwrap() = true;
    }

    /// `attach`, `shutdown` and `kill` calls so far, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Backend for StubBackend {
    fn launch(&self, _spec: &MachineSpec) -> Result<Box<dyn Machine>> {
        Err(Error::new(ErrorKind::Internal, "the stub backend launches nothing"))
    }

    fn attach(&self, rec: &MachineRecord) -> Result<Box<dyn Machine>> {
        self.calls.lock().unwrap().push("attach".to_owned());
        match self.attach.lock().unwrap().clone() {
            Attach::Running => Ok(Box::new(StubMachine {
                record: rec.clone(),
                fails: *self.stop_fails.lock().unwrap(),
                calls: self.calls.clone(),
            })),
            Attach::Gone => Err(Error::new(ErrorKind::NotFound, "the stub machine is gone")),
            Attach::Fails(kind) => Err(Error::new(kind, "the stub backend cannot look")),
        }
    }

    fn storage(&self) -> &dyn Storage {
        &self.storage
    }

    fn can_save_memory(&self) -> bool {
        false
    }
}

struct StubMachine {
    record: MachineRecord,
    fails: bool,
    calls: Arc<Mutex<Vec<String>>>,
}

impl StubMachine {
    fn stop(&self, call: &str) -> Result<()> {
        self.calls.lock().unwrap().push(call.to_owned());
        if self.fails {
            Err(Error::new(ErrorKind::Io, format!("the stub machine refuses to {call}")))
        } else {
            Ok(())
        }
    }
}

impl Machine for StubMachine {
    fn record(&self) -> MachineRecord {
        self.record.clone()
    }

    fn ssh_endpoints(&self) -> &[(Endpoint, SocketAddr)] {
        &[]
    }

    fn serial_log(&self) -> &Path {
        &self.record.serial_log
    }

    fn resume(&mut self) -> Result<()> {
        Err(Error::new(
            ErrorKind::Internal,
            "a stub machine was found, not launched",
        ))
    }

    fn release(&mut self) -> Result<()> {
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        self.stop("shutdown")
    }

    fn kill(&mut self) -> Result<()> {
        self.stop("kill")
    }
}
