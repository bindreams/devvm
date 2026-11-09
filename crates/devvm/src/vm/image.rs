//! Images, and the VMs started from them.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use crate::access::SshSession;
use crate::access::sftp::Sftp;
use crate::backend::{Machine, ShutdownOutcome};
use crate::contain::Lifetime;
use crate::engine::Engine;
use crate::exit::{ExitStatus, Output};
use crate::guest::GuestOs;
use crate::host::Unavailable;
use crate::instance::dir::OpShared;
use crate::instance::name::InstanceName;
use crate::instance::registry::{NewInstance, Owned};
use crate::recipe::{Flattened, Recipe};
use crate::store::fetch::LazyHttp;
use crate::store::layers::ChainLease;
use crate::vm::{Guest, Handle, Instance, Started, ops, remove_leftovers, remove_ssh_entry, warn_for};
use crate::{Arch, BuildOptions, Command, Error, ErrorKind, GuestKind, Result, StartOptions};

#[cfg(test)]
#[path = "image_tests.rs"]
mod image_tests;

#[cfg(all(test, windows))]
#[path = "image_windows_tests.rs"]
mod image_windows_tests;

#[cfg(all(test, not(windows)))]
#[path = "image_other_tests.rs"]
mod image_other_tests;

const DEFAULT_CPUS: u32 = 2;
const DEFAULT_MEMORY: u64 = 4 << 30;

/// A built image: the layers a VM starts from. While the handle lives, garbage collection keeps those layers.
pub struct Image {
    engine: Arc<Engine>,
    /// The layers a VM starts from, root first.
    pub(crate) chain: ChainLease,
    guest: GuestKind,
    arch: Arch,
    cpus: u32,
    memory: u64,
    disk: Option<u64>,
}

impl Image {
    /// The image whose layers are `chain`, built for `arch`, with the defaults `flat` declares.
    pub(crate) fn from_chain(engine: Arc<Engine>, chain: ChainLease, flat: &Flattened, arch: Arch) -> Image {
        Image {
            engine,
            chain,
            guest: flat.guest,
            arch,
            cpus: flat.resources.cpus.unwrap_or(DEFAULT_CPUS),
            memory: flat.resources.memory.unwrap_or(DEFAULT_MEMORY),
            disk: flat.resources.disk,
        }
    }

    /// Starts a VM that lives as long as the returned handle: it dies with this process and is destroyed when the
    /// handle is dropped. Returns once the guest answers over SSH.
    pub fn start(&self, opts: &StartOptions) -> Result<Vm> {
        self.start_vm(None, opts, Lifetime::DiesWithOwner)
    }

    /// Creates the instance `name`, starts its VM and returns once the guest answers over SSH. The VM and the instance
    /// stay until `Instance::remove`, whether or not this process lives on. A failure removes what it made, except a
    /// VM already released for the hand-over: the reaper takes that one.
    pub fn up(&self, name: &str, opts: &StartOptions) -> Result<Instance> {
        self.up_with(name, opts, &mut || {})
    }

    /// `up`, running `after_disarm` once the machine's containment is disarmed and before the owner marker is flipped.
    pub(crate) fn up_with(&self, name: &str, opts: &StartOptions, after_disarm: &mut dyn FnMut()) -> Result<Instance> {
        let name = InstanceName::parse(name)?;
        self.start_vm(Some(name), opts, Lifetime::OutlivesOwner)?
            .hand_over(after_disarm)
    }

    /// `start` with an instance name (`None`: an unnamed one) and a given VM lifetime. `OutlivesOwner` is `up`: the VM
    /// is handed over (`Vm::hand_over`) once it is ready, and only then outlives this process.
    pub(crate) fn start_vm(&self, name: Option<InstanceName>, opts: &StartOptions, lifetime: Lifetime) -> Result<Vm> {
        let owned = self.create_owned(name, opts)?;
        // From here on the handle cleans up after any failure.
        let mut vm = Vm {
            instance: Some(Instance {
                engine: self.engine.clone(),
                handle: Handle::Owned(owned),
            }),
            machine: Mutex::new(None),
            session: Mutex::new(None),
        };
        let Started { machine, session } = vm.instance().start_with(lifetime)?.ok_or_else(|| {
            Error::new(
                ErrorKind::Internal,
                format!("the new instance {} is already running", vm.name()),
            )
        })?;
        *vm.machine.lock().unwrap_or_else(PoisonError::into_inner) = Some(machine);
        *vm.session.get_mut().unwrap_or_else(PoisonError::into_inner) = Some(session);
        Ok(vm)
    }

    /// Creates an instance called `name`, not started, that stays until it is removed.
    pub fn create(&self, name: &str, opts: &StartOptions) -> Result<Instance> {
        let name = InstanceName::parse(name)?;
        let dir = self.create_owned(Some(name), opts)?.make_ownerless(&mut || Ok(()))?;
        Ok(Instance {
            engine: self.engine.clone(),
            handle: Handle::Open(dir),
        })
    }

    /// A new instance on this image, owned by this process.
    fn create_owned(&self, name: Option<InstanceName>, opts: &StartOptions) -> Result<Owned> {
        let backend = self.engine.backend()?;
        let created_from = std::env::current_dir()
            .map_err(|e| Error::io(ErrorKind::Io, "cannot tell which directory this command runs in", e))?;
        // The chain is already leased by this handle; the guard keeps garbage collection out until the record names it.
        let guard = self.engine.store()?.begin_record()?;
        self.engine.registry().create(
            NewInstance {
                name,
                chain: &self.chain,
                guest: self.guest,
                arch: self.arch,
                disk_size: self.disk,
                cpus: opts.cpus.unwrap_or(self.cpus),
                memory: opts.memory.unwrap_or(self.memory),
                network: opts.network,
                created_from,
                created: self.engine.clock().now(),
                storage: backend.storage(),
            },
            &guard,
        )
    }
}

impl Recipe {
    /// Builds the image this recipe describes: one cached layer for its base and one per step. A layer already built
    /// from the same inputs is reused. See [`Engine::build`].
    pub fn build(&self, opts: &BuildOptions) -> Result<Image> {
        Engine::from_env_stderr(Arc::new(LazyHttp::new()))?.build(self, opts)
    }
}

/// Whether this host can run VMs; what is missing otherwise. An engine that cannot be made (an unreadable
/// `config.yaml`) is what is missing.
pub fn availability() -> std::result::Result<(), Unavailable> {
    Engine::from_env_stderr(Arc::new(LazyHttp::new()))
        .map_err(|e| Unavailable {
            missing: e.message().to_owned(),
        })?
        .availability()
}

/// A VM that lives as long as this handle. Dropping it kills the machine and destroys the instance; if the instance is
/// busy or the destruction fails, a warning names it and the next devvm command finishes the removal.
pub struct Vm {
    /// `None` once handed over (`hand_over`): nothing is left to clean up.
    instance: Option<Instance>,
    machine: Mutex<Option<Box<dyn Machine>>>,
    /// The connection that found the guest ready, which every operation uses; `None` until the start has finished. A
    /// reboot replaces it with the one that found the new boot ready. A reboot that failed leaves `None`: the old
    /// connection may still look alive while it is to the old boot. An operation connects again for a session that is
    /// `None` or no longer alive.
    session: Mutex<Option<SshSession>>,
}

impl Vm {
    pub(crate) fn instance(&self) -> &Instance {
        self.instance.as_ref().expect("a live Vm has its instance")
    }

    pub(crate) fn name(&self) -> &str {
        self.instance().name()
    }

    /// The handover of `up`: disarms the machine's containment, then makes the instance ownerless, so VM and instance
    /// stay until `rm`. `after_disarm` runs between the two steps. If either fails, the VM is killed or (when it was
    /// already released) left for the reaper, which takes it by its recorded identity.
    pub(crate) fn hand_over(mut self, after_disarm: &mut dyn FnMut()) -> Result<Instance> {
        let instance = self.instance.take();
        let machine = self.machine.get_mut().unwrap_or_else(PoisonError::into_inner).take();
        let (Some(Instance { engine, handle }), Some(mut machine)) = (instance, machine) else {
            return Err(Error::new(ErrorKind::Internal, "this VM was already handed over"));
        };
        let Handle::Owned(owned) = handle else {
            return Err(Error::new(
                ErrorKind::Internal,
                "this VM's instance is not owned by this process",
            ));
        };
        let dir = owned.make_ownerless(&mut || {
            machine.release()?;
            after_disarm();
            Ok(())
        })?;
        Ok(Instance {
            engine,
            handle: Handle::Open(dir),
        })
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let Some(instance) = self.instance.as_ref() else {
            return;
        };
        let dir = instance.dir();
        let progress = instance.engine.progress();
        let machine = self.machine.get_mut().unwrap_or_else(PoisonError::into_inner).take();
        let dead = match machine {
            Some(mut machine) => {
                if let Err(e) = machine.kill() {
                    progress.message(&format!(
                        "instance {} could not be removed: its VM could not be killed ({}); the next devvm command \
                         retries",
                        dir.name(),
                        e.message()
                    ));
                    return;
                }
                Some(machine.record())
            }
            None => None,
        };
        let engine = &instance.engine;
        // Without the op lock the record stays, but the dead machine's files go all the same.
        let leftovers_only = || {
            if let Some(dead) = &dead {
                remove_leftovers(engine, dir, dead);
            }
        };
        match dir.try_op_exclusive() {
            Ok(Some(op)) => {
                if let Some(dead) = &dead
                    && let Err(e) = dir.retire_machine(&op, dead, engine.host(), &mut warn_for(engine, dir))
                {
                    progress.message(&format!(
                        "instance {} still has a record naming its dead VM ({}); the next devvm command clears it",
                        dir.name(),
                        e.message()
                    ));
                }
                remove_ssh_entry(engine, dir);
                if let Err(e) = dir.destroy(op) {
                    progress.message(&format!(
                        "instance {} could not be removed ({}); the next devvm command retries",
                        dir.name(),
                        e.message()
                    ));
                }
            }
            Ok(None) => {
                leftovers_only();
                progress.message(&format!(
                    "instance {} is busy with another devvm command and was not removed; the next devvm command \
                     removes it",
                    dir.name()
                ));
            }
            // Already removed by someone else.
            Err(e) if e.kind() == ErrorKind::NotFound => leftovers_only(),
            Err(e) => {
                leftovers_only();
                progress.message(&format!(
                    "instance {} could not be removed ({}); the next devvm command retries",
                    dir.name(),
                    e.message()
                ));
            }
        }
    }
}

impl Vm {
    /// Runs `f` with an SFTP session to the guest, over the connection that found it ready, under the shared op lock
    /// that keeps `stop`, `remove` and `reboot` from ending the machine while `f` uses it.
    pub(crate) fn with_sftp<T>(&self, f: impl FnOnce(&Sftp) -> Result<T>) -> Result<T> {
        let (_, session, _op) = self.reached()?;
        f(&session.sftp()?)
    }

    /// Shuts the guest down and returns once its process has exited, under the exclusive op lock. The VM is not
    /// running afterwards, and the handle can no longer run anything. A shutdown that fails leaves the machine for
    /// the drop to kill: it is never forced here. Only `PoweredDown` says the guest shut down cleanly; for any other
    /// outcome what its disk holds is not a clean shutdown's, and the machine's record is retired all the same.
    pub(crate) fn shutdown(&self) -> Result<ShutdownOutcome> {
        let instance = self.instance();
        let dir = instance.dir();
        let op = dir.op_exclusive()?;
        let mut slot = self.machine.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(machine) = slot.as_mut() else {
            return Err(Error::new(ErrorKind::Internal, "this VM is no longer running"));
        };
        // A connection to a guest that powers down is of no further use.
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let outcome = machine.shutdown()?;
        let dead = slot.take().expect("the machine was just shut down").record();
        dir.retire_machine(&op, &dead, instance.engine.host(), &mut warn_for(&instance.engine, dir))?;
        Ok(outcome)
    }

    /// The guest, the session and the shared op lock for one operation.
    fn reached(&self) -> Result<(Arc<dyn GuestOs>, SshSession, OpShared)> {
        let instance = self.instance();
        let dir = instance.dir();
        let op = dir.op_shared()?;
        let guest = ops::guest_of(&instance.engine, dir)?;
        let mut slot = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        let session = match slot.as_ref().filter(|session| session.is_alive()) {
            Some(session) => session.clone(),
            None => slot.insert(ops::reach(&instance.engine, dir, &op)?.1).clone(),
        };
        Ok((guest, session, op))
    }

    /// Runs `cmd` in the guest, writing its output to `out` and `err` as it arrives. As [`Instance::run_to`].
    pub fn run_to(&self, cmd: &Command, out: &mut dyn Write, err: &mut dyn Write) -> Result<ExitStatus> {
        let (guest, session, _op) = self.reached()?;
        ops::run_to(&self.instance().engine, &*guest, &session, cmd, out, err)
    }
}

impl Guest for Vm {
    fn run(&self, cmd: &Command) -> Result<Output> {
        let (guest, session, _op) = self.reached()?;
        ops::run(&*guest, &session, cmd)
    }

    fn reboot(&self) -> Result<()> {
        let instance = self.instance();
        let dir = instance.dir();
        let op = dir.op_exclusive()?;
        let session = ops::reboot(&instance.engine, dir, &op);
        // After a failed reboot the old session can still look alive while it points at the old boot: drop it.
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = session.as_ref().ok().cloned();
        session.map(drop)
    }

    fn copy_in(&self, src: &Path, dest: &str) -> Result<()> {
        let (guest, session, _op) = self.reached()?;
        ops::copy_in(&self.instance().engine, &guest, &session, src, dest)
    }

    fn copy_out(&self, src: &str, dest: &Path) -> Result<()> {
        let (guest, session, _op) = self.reached()?;
        ops::copy_out(&self.instance().engine, &guest, &session, src, dest)
    }
}
