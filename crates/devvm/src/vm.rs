//! The public handles: [`Instance`] now; `Image` and `Vm` follow with the backend.

use std::sync::Arc;

use crate::backend::Machine;
use crate::contain::{Lifetime, VmProcess};
use crate::engine::Engine;
use crate::instance::dir::InstanceDir;
use crate::instance::name::InstanceName;
use crate::{ErrorKind, Result};

mod launch;

#[cfg(test)]
#[path = "vm_tests.rs"]
pub(crate) mod vm_tests;

#[cfg(all(test, windows))]
#[path = "vm_windows_tests.rs"]
mod vm_windows_tests;

#[cfg(all(test, not(windows)))]
#[path = "vm_other_tests.rs"]
mod vm_other_tests;

/// A named VM that stays until it is removed.
pub struct Instance {
    engine: Arc<Engine>,
    dir: InstanceDir,
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
            dir,
        })
    }

    pub fn name(&self) -> &str {
        self.dir.name().as_str()
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
        let op = self.dir.op_exclusive()?;
        launch::launch(&self.engine, &self.dir, &op, lifetime)
    }

    /// Shuts the VM down and returns once its process has exited. Does nothing to an instance that is not running.
    /// A shutdown that fails is an error: `remove` is the forceful way.
    pub fn stop(&self) -> Result<()> {
        let dir = &self.dir;
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
        dir.update(&op, &mut |record| record.machine = None)?;
        Ok(())
    }

    /// Kills the VM by its recorded identity and deletes the instance. Waits for other commands working on it; if
    /// another command removed it first, that is `not-found`.
    pub fn remove(self) -> Result<()> {
        let dir = &self.dir;
        let op = dir.op_exclusive()?;
        let record = dir.record()?;
        if let Some(machine) = &record.machine {
            if let Some(mut vm) = VmProcess::attach(&machine.process)? {
                vm.kill()?;
            }
            // The VM is dead: a destroy that fails below must not leave a record naming it.
            dir.update(&op, &mut |record| record.machine = None)?;
        }
        dir.destroy(op)?;
        self.engine
            .progress()
            .message(&format!("removed instance {}", dir.name()));
        Ok(())
    }
}
