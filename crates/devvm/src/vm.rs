//! The public handles: [`Image`] (what instances are made from), [`Vm`] (a VM that dies with its handle) and
//! [`Instance`] (a named VM that stays until it is removed).

use std::path::Path;
use std::sync::Arc;

use crate::backend::{Machine, MachineRecord};
use crate::contain::{Lifetime, VmProcess};
use crate::engine::Engine;
use crate::exit::Output;
use crate::instance::dir::InstanceDir;
use crate::instance::name::InstanceName;
use crate::instance::registry::Owned;
use crate::{Command, Error, ErrorKind, Result};

#[allow(
    dead_code,
    reason = "`Recipe::build` (U4) is the first non-test caller of `Image::from_chain`, and `up` (U4) of `Vm::hand_over`"
)]
mod image;
mod launch;

pub use image::{Image, Vm, availability};

#[cfg(test)]
#[path = "vm_tests.rs"]
pub(crate) mod vm_tests;

#[cfg(all(test, windows))]
#[path = "vm_windows_tests.rs"]
mod vm_windows_tests;

#[cfg(all(test, not(windows)))]
#[path = "vm_other_tests.rs"]
mod vm_other_tests;

/// Running commands in a guest and moving files in and out of it. Needs the access layer, which arrives with the QEMU
/// backend: until then every method is `not-supported`.
pub trait Guest {
    fn run(&self, cmd: &Command) -> Result<Output>;
    fn reboot(&self) -> Result<()>;
    fn copy_in(&self, src: &Path, dest: &str) -> Result<()>;
    fn copy_out(&self, src: &str, dest: &Path) -> Result<()>;
}

/// Deletes what `machine` left outside the instance directory. Call it only once the machine is proven dead; a file
/// that cannot be deleted is a warning naming the instance.
pub(crate) fn remove_leftovers(engine: &Engine, dir: &InstanceDir, machine: &MachineRecord) {
    machine.remove_leftovers(&mut |text| engine.progress().message(&format!("instance {}: {text}", dir.name())));
}

pub(crate) fn no_access_layer() -> Error {
    Error::new(
        ErrorKind::NotSupported,
        "reaching the guest needs the access layer, which arrives with the QEMU backend",
    )
}

/// A named VM that stays until it is removed.
pub struct Instance {
    engine: Arc<Engine>,
    handle: Handle,
}

/// How an `Instance` reaches its directory: opened by name, or created by this process, which then still owns it.
enum Handle {
    Open(InstanceDir),
    Owned(Owned),
}

impl Instance {
    /// The instance called `name` (case-insensitive). Reaps leftovers of dead devvm processes first.
    pub fn open(name: &str) -> Result<Instance> {
        let name = InstanceName::parse(name)?;
        let engine = Engine::from_env_stderr()?;
        Instance::open_in(&engine, name.as_str())
    }

    /// `open` in a given engine. The name is validated before anything is read or created; leftovers are reaped
    /// first, even when the name turns out to be missing, so that every library entry point cleans up after killed
    /// processes.
    pub(crate) fn open_in(engine: &Arc<Engine>, name: &str) -> Result<Instance> {
        let name = InstanceName::parse(name)?;
        engine.reap();
        let dir = engine.registry().open(&name)?;
        Ok(Instance {
            engine: engine.clone(),
            handle: Handle::Open(dir),
        })
    }

    pub(crate) fn dir(&self) -> &InstanceDir {
        match &self.handle {
            Handle::Open(dir) => dir,
            Handle::Owned(owned) => owned.dir(),
        }
    }

    pub fn name(&self) -> &str {
        self.dir().name().as_str()
    }

    /// Starts the VM and leaves it running after this process exits. Does nothing if it already runs. Waits for other
    /// commands working on the instance; `not-found` if one of them removed it.
    ///
    /// Returns once the instance record names the machine and the machine has been released from this process.
    /// Frozen spawn needs Windows in 0.1.0, so elsewhere this is `not-supported`.
    pub fn start(&self) -> Result<()> {
        match self.start_with(Lifetime::OutlivesOwner)? {
            Some(mut machine) => machine.release(),
            None => Ok(()),
        }
    }

    /// Launches under the exclusive op lock; `None` when the machine already runs. The machine is resumed and still
    /// contained: the caller releases it (`start`) or keeps it (`Image::start`, `up`).
    pub(crate) fn start_with(&self, lifetime: Lifetime) -> Result<Option<Box<dyn Machine>>> {
        let dir = self.dir();
        let op = dir.op_exclusive()?;
        launch::launch(&self.engine, dir, &op, lifetime)
    }

    /// Shuts the VM down and returns once its process has exited. Does nothing to an instance that is not running.
    /// A shutdown that fails is an error: `remove` is the forceful way.
    pub fn stop(&self) -> Result<()> {
        let dir = self.dir();
        let op = dir.op_exclusive()?;
        let Some(machine) = dir.record()?.machine else {
            return Ok(());
        };
        match self.engine.backend()?.attach(&machine) {
            Ok(mut running) => running.shutdown()?,
            // Not running any more: the record is stale.
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        // The machine is dead either way, and what it left behind can go.
        remove_leftovers(&self.engine, dir, &machine);
        dir.update(&op, &mut |record| record.machine = None)?;
        Ok(())
    }

    /// Kills the VM by its recorded identity and deletes the instance. Waits for other commands working on it; if
    /// another command removed it first, that is `not-found`.
    pub fn remove(self) -> Result<()> {
        let dir = self.dir();
        let op = dir.op_exclusive()?;
        let record = dir.record()?;
        if let Some(machine) = &record.machine {
            if let Some(mut vm) = VmProcess::attach(&machine.process)? {
                vm.kill()?;
            }
            // The VM is dead: what it left outside the directory goes too, and a destroy that fails below must not
            // leave a record naming it.
            remove_leftovers(&self.engine, dir, machine);
            dir.update(&op, &mut |record| record.machine = None)?;
        }
        dir.destroy(op)?;
        self.engine
            .progress()
            .message(&format!("removed instance {}", dir.name()));
        Ok(())
    }
}

impl Guest for Instance {
    fn run(&self, _cmd: &Command) -> Result<Output> {
        Err(no_access_layer())
    }

    fn reboot(&self) -> Result<()> {
        Err(no_access_layer())
    }

    fn copy_in(&self, _src: &Path, _dest: &str) -> Result<()> {
        Err(no_access_layer())
    }

    fn copy_out(&self, _src: &str, _dest: &Path) -> Result<()> {
        Err(no_access_layer())
    }
}
