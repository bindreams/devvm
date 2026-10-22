//! The public handles: [`Instance`] now; `Image` and `Vm` follow with the backend.

use std::sync::Arc;

use crate::Result;
use crate::contain::VmProcess;
use crate::engine::Engine;
use crate::instance::dir::InstanceDir;
use crate::instance::name::InstanceName;

#[cfg(test)]
#[path = "vm_tests.rs"]
mod vm_tests;

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
