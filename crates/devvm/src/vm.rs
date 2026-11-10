//! The public handles: [`Image`] (what instances are made from), [`Vm`] (a VM that dies with its handle) and
//! [`Instance`] (a named VM that stays until it is removed).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::access::SshSession;
use crate::backend::MachineRecord;
use crate::contain::Lifetime;
use crate::engine::Engine;
use crate::engine::keys::read_target;
use crate::exit::{ExitStatus, Output};
use crate::guest::GuestOs;
use crate::host::symlink_metadata_if_present;
use crate::instance::dir::{InstanceDir, OpShared};
use crate::instance::name::InstanceName;
use crate::instance::registry::Owned;
use crate::store::fetch::LazyHttp;
use crate::{Command, Error, ErrorKind, Result};

mod image;
mod launch;
mod ops;

pub use image::{Image, Vm, availability};
pub(crate) use launch::Started;

#[cfg(test)]
#[path = "vm_tests.rs"]
pub(crate) mod vm_tests;

#[cfg(all(test, windows))]
#[path = "vm_windows_tests.rs"]
mod vm_windows_tests;

#[cfg(all(test, not(windows)))]
#[path = "vm_other_tests.rs"]
mod vm_other_tests;

/// Running commands in a guest and moving files in and out of it. Each method waits for the commands that change the
/// instance (`start`, `stop`, `remove`, `reboot`) and fails with `unavailable` when the VM is not running. A VM that
/// dies while a method works can fail it with `connection-lost` instead.
pub trait Guest {
    fn run(&self, cmd: &Command) -> Result<Output>;
    fn reboot(&self) -> Result<()>;
    /// Copies the host file, symlink or directory `src` to the guest path `dest`.
    ///
    /// Contract: a copied tree has no two names that differ only in case, no name ending in a dot or a space and no
    /// Windows device name. Nothing checks this, and a tree that breaks it may be copied wrongly.
    fn copy_in(&self, src: &Path, dest: &str) -> Result<()>;
    /// Copies the guest file, symlink or directory `src` to the host path `dest`. Same contract as [`Guest::copy_in`].
    fn copy_out(&self, src: &str, dest: &Path) -> Result<()>;
}

/// Deletes what `machine` left outside the instance directory. Call it only once the machine is proven dead; a file
/// that cannot be deleted is a warning naming the instance.
pub(crate) fn remove_leftovers(engine: &Engine, dir: &InstanceDir, machine: &MachineRecord) {
    machine.remove_leftovers(engine.host(), &mut warn_for(engine, dir));
}

/// Removes the `ssh_config` entry of `dir`'s instance; a failure is a warning naming the instance. Call it before
/// `destroy`, while the name is still this instance's: once it is released, another command may place an instance of
/// the same name and write its entry, which this one must not remove.
pub(crate) fn remove_ssh_entry(engine: &Engine, dir: &InstanceDir) {
    if let Err(e) = engine.ssh_configs().remove_entry(dir.name()) {
        warn_for(engine, dir)(format!("cannot remove the ssh configuration entry: {}", e.message()));
    }
}

/// Reports a problem with the files of a dead machine of `dir` on the progress channel, naming the instance.
pub(crate) fn warn_for<'a>(engine: &'a Engine, dir: &'a InstanceDir) -> impl FnMut(String) + 'a {
    move |text| engine.progress().message(&format!("instance {}: {text}", dir.name()))
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
        let engine = Engine::from_env_stderr(Arc::new(LazyHttp::new()))?;
        Instance::open_in(&engine, name.as_str())
    }

    /// `open` in a given engine.
    pub(crate) fn open_in(engine: &Arc<Engine>, name: &str) -> Result<Instance> {
        let name = InstanceName::parse(name)?;
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
    /// Returns once the guest has finished booting and answers over SSH, the instance record names the machine and the
    /// machine has been released from this process. A guest that does not answer fails the start and is killed.
    /// Frozen spawn needs Windows in 0.1.0, so elsewhere this is `not-supported`.
    pub fn start(&self) -> Result<()> {
        let dir = self.dir();
        let op = dir.op_exclusive()?;
        let Some(Started { mut machine, .. }) = launch::launch(&self.engine, dir, &op, Lifetime::OutlivesOwner)? else {
            return Ok(());
        };
        // Released under the op lock: until then the machine dies with this process, and a second `start` must not
        // find it running.
        machine
            .release()
            .map_err(|e| launch::abandon(&self.engine, dir, &op, &mut *machine, e))
    }

    /// Launches under the exclusive op lock and waits for the guest; `None` when the machine already runs. The machine
    /// is resumed and still contained: the caller keeps it, and the session that found the guest ready.
    pub(crate) fn start_with(&self, lifetime: Lifetime) -> Result<Option<Started>> {
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
        // A machine the backend does not find is not running: the record is stale.
        if let Some(mut running) = self.engine.backend()?.find(&machine)? {
            running.shutdown()?;
        }
        dir.retire_machine(&op, &machine, self.engine.host(), &mut warn_for(&self.engine, dir))
    }

    /// Kills the VM by its recorded identity and deletes the instance. Waits for other commands working on it; if
    /// another command removed it first, that is `not-found`.
    pub fn remove(self) -> Result<()> {
        let dir = self.dir();
        let op = dir.op_exclusive()?;
        let record = dir.record()?;
        if let Some(machine) = &record.machine {
            if let Some(mut running) = self.engine.backend()?.find(machine)? {
                running.kill()?;
            }
            // A destroy that fails below must not leave a record naming a dead machine.
            dir.retire_machine(&op, machine, self.engine.host(), &mut warn_for(&self.engine, dir))?;
        }
        remove_ssh_entry(&self.engine, dir);
        dir.destroy(op)?;
        self.engine
            .progress()
            .message(&format!("removed instance {}", dir.name()));
        Ok(())
    }

    /// The `ssh_config` file through which the system `ssh` reaches this instance by its name. Fails with
    /// `unavailable`, as `run_to` does, when the instance's machine is not running, or when the file or the
    /// instance's entry in it is missing: `ssh` would then fail, or look the name up as a host.
    ///
    /// No lock is held while `ssh` runs: a `stop` or `rm` ends the session.
    pub fn ssh_config(&self) -> Result<PathBuf> {
        let dir = self.dir();
        let op = dir.op_shared()?;
        read_target(&self.engine, dir, &op)?;
        let configs = self.engine.ssh_configs();
        let config = configs.config();
        for file in [config.clone(), configs.conf_path(dir.name())] {
            let present = symlink_metadata_if_present(&file)
                .map_err(|e| Error::io(ErrorKind::Io, format!("cannot look at {}", file.display()), e))?;
            if present.is_none() {
                return Err(Error::new(
                    ErrorKind::Unavailable,
                    format!(
                        "the ssh configuration of instance {} is missing {}: stop and start it",
                        dir.name(),
                        file.display()
                    ),
                ));
            }
        }
        Ok(config)
    }
}

impl Instance {
    /// The guest of this instance and a new connection to it, with the shared op lock that keeps `stop`, `remove` and
    /// `reboot` from ending the machine under the caller.
    fn reached(&self) -> Result<(Arc<dyn GuestOs>, SshSession, OpShared)> {
        let dir = self.dir();
        let op = dir.op_shared()?;
        let guest = ops::guest_of(&self.engine, dir)?;
        let (_, session) = ops::reach(&self.engine, dir, &op)?;
        Ok((guest, session, op))
    }

    /// Runs `cmd` in the guest, writing its output to `out` and `err` as it arrives, and returns how it ended.
    ///
    /// Output the guest sent after the command's status that devvm cannot carry is reported on the progress channel;
    /// the status stands. A writer that fails is an `io` error, and the guest command is stopped, unless the command had
    /// already ended: the error then names its status.
    pub fn run_to(&self, cmd: &Command, out: &mut dyn Write, err: &mut dyn Write) -> Result<ExitStatus> {
        let (guest, session, _op) = self.reached()?;
        ops::run_to(&self.engine, &*guest, &session, cmd, out, err)
    }
}

impl Guest for Instance {
    fn run(&self, cmd: &Command) -> Result<Output> {
        let (guest, session, _op) = self.reached()?;
        ops::run(&*guest, &session, cmd)
    }

    fn reboot(&self) -> Result<()> {
        let dir = self.dir();
        let op = dir.op_exclusive()?;
        ops::reboot(&self.engine, dir, &op).map(drop)
    }

    fn copy_in(&self, src: &Path, dest: &str) -> Result<()> {
        let (guest, session, _op) = self.reached()?;
        ops::copy_in(&self.engine, &guest, &session, src, dest)
    }

    fn copy_out(&self, src: &str, dest: &Path) -> Result<()> {
        let (guest, session, _op) = self.reached()?;
        ops::copy_out(&self.engine, &guest, &session, src, dest)
    }
}
