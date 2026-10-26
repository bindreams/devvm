//! Images, and the VMs started from them.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use crate::backend::Machine;
use crate::contain::Lifetime;
use crate::engine::Engine;
use crate::exit::Output;
use crate::host::{self, Unavailable};
use crate::instance::name::InstanceName;
use crate::instance::registry::{NewInstance, Owned};
use crate::recipe::{Flattened, Recipe};
use crate::store::layers::ChainLease;
use crate::vm::{Guest, Handle, Instance, no_access_layer, remove_leftovers, warn_for};
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
const DEFAULT_DISK: u64 = 40 << 30;

/// A built image: the layers a VM starts from. While the handle lives, garbage collection keeps those layers.
pub struct Image {
    engine: Arc<Engine>,
    chain: ChainLease,
    guest: GuestKind,
    arch: Arch,
    cpus: u32,
    memory: u64,
    disk: u64,
}

impl Image {
    /// The image whose layers are `chain`, with the defaults `flat` declares.
    pub(crate) fn from_chain(engine: Arc<Engine>, chain: ChainLease, flat: &Flattened) -> Image {
        Image {
            engine,
            chain,
            guest: flat.guest,
            arch: Arch::host(),
            cpus: flat.resources.cpus.unwrap_or(DEFAULT_CPUS),
            memory: flat.resources.memory.unwrap_or(DEFAULT_MEMORY),
            disk: flat.resources.disk.unwrap_or(DEFAULT_DISK),
        }
    }

    /// Starts a VM that lives as long as the returned handle: it dies with this process and is destroyed when the
    /// handle is dropped. Reaps the leftovers of dead devvm processes first.
    pub fn start(&self, opts: &StartOptions) -> Result<Vm> {
        self.start_vm(opts, Lifetime::DiesWithOwner)
    }

    /// `start` with a given VM lifetime. `OutlivesOwner` is `up`: the VM is handed over (`Vm::hand_over`) once it is
    /// ready, and only then outlives this process.
    pub(crate) fn start_vm(&self, opts: &StartOptions, lifetime: Lifetime) -> Result<Vm> {
        self.engine.reap();
        let owned = self.create_owned(None, opts)?;
        // From here on the handle cleans up after any failure.
        let vm = Vm {
            instance: Some(Instance {
                engine: self.engine.clone(),
                handle: Handle::Owned(owned),
            }),
            machine: Mutex::new(None),
        };
        let machine = vm.instance().start_with(lifetime)?.ok_or_else(|| {
            Error::new(
                ErrorKind::Internal,
                format!("the new instance {} is already running", vm.name()),
            )
        })?;
        *vm.machine.lock().unwrap_or_else(PoisonError::into_inner) = Some(machine);
        Ok(vm)
    }

    /// Creates an instance called `name`, not started, that stays until it is removed. Reaps the leftovers of dead
    /// devvm processes first.
    pub fn create(&self, name: &str, opts: &StartOptions) -> Result<Instance> {
        let name = InstanceName::parse(name)?;
        self.engine.reap();
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
    /// Builds the image this recipe describes. Needs the QEMU backend, so it is `not-supported` until that arrives.
    pub fn build(&self, _opts: &BuildOptions) -> Result<Image> {
        Err(Error::new(
            ErrorKind::NotSupported,
            "building images needs the QEMU backend, which arrives with the next devvm release",
        ))
    }
}

/// Whether this host can run VMs; what is missing otherwise.
pub fn availability() -> std::result::Result<(), Unavailable> {
    host::current().accelerator(Arch::host()).map(drop)
}

/// A VM that lives as long as this handle. Dropping it kills the machine and destroys the instance; if the instance is
/// busy or the destruction fails, a warning names it and the next devvm command finishes the removal.
pub struct Vm {
    /// `None` once handed over (`hand_over`): nothing is left to clean up.
    instance: Option<Instance>,
    machine: Mutex<Option<Box<dyn Machine>>>,
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
                    && let Err(e) = dir.retire_machine(&op, dead, &mut warn_for(engine, dir))
                {
                    progress.message(&format!(
                        "instance {} still has a record naming its dead VM ({}); the next devvm command clears it",
                        dir.name(),
                        e.message()
                    ));
                }
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

impl Guest for Vm {
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
