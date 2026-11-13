//! The engine: the collaborators every devvm operation needs (directories, host, backend, layer store, instance
//! registry), created once per process.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use cosca::identity::ProcessIdRecord;

use crate::access::ssh_config::SshConfigDir;
use crate::access::{VmExit, WatchEnd};
use crate::backend::qemu::sidecar::LazyInstall;
use crate::backend::qemu::{QemuParts, qemu_parts};
use crate::backend::{Backend, MachineRecord};
use crate::contain::VmProcess;
use crate::exit::{Progress, StderrProgress};
use crate::guest::Guests;
use crate::host::{self, Host};
use crate::instance::dir::InstanceDir;
use crate::instance::expiry::warn_if_expired;
use crate::instance::info::{InstanceInfo, InstanceState};
use crate::instance::name::InstanceName;
use crate::instance::record::{MarkerRead, Owner, RecordRead};
use crate::instance::registry::Registry;
use crate::lazy::Lazy;
use crate::store::fetch::{Fetcher, Transport};
use crate::store::layers::{Clock, LayerStore, SystemClock};
use crate::{Dirs, Error, ErrorKind, Instance, Result};

mod build;
mod build_dir;
mod checks;
pub(crate) mod keys;
pub(crate) mod ready;

#[cfg(test)]
#[path = "engine_tests.rs"]
pub(crate) mod engine_tests;

#[cfg(test)]
#[path = "engine_qemu_tests.rs"]
mod engine_qemu_tests;

/// How long a guest gets to boot and answer before a start fails: a failure bound on an event outside devvm. Read by
/// every start, which waits for the guest to answer.
pub(crate) const DEFAULT_BOOT_BOUND: Duration = Duration::from_secs(10 * 60);

/// Which instances `list` returns.
#[derive(Clone, Debug)]
pub enum ListFilter {
    All,
    /// Instances created from this directory. Directories are compared by canonical path; one that no longer exists
    /// matches by the path as written.
    CreatedIn(PathBuf),
}

/// Where a launch is, for a test that must stop a devvm process at an exact step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchStep {
    /// The machine exists, frozen, and nothing records it yet.
    Spawned,
    /// The instance record names the machine; it is still frozen.
    Recorded,
    /// The machine runs; it is still contained.
    Resumed,
}

/// Called by `Instance::start_with` after each step. The default does nothing; a test parks here on a real primitive.
pub(crate) trait LaunchHooks: Send + Sync {
    fn reached(&self, _step: LaunchStep, _machine: &MachineRecord) {}
}

pub(crate) struct NoHooks;

impl LaunchHooks for NoHooks {}

/// Everything an [`Engine`] is made of; tests inject fakes.
pub(crate) struct EngineParts {
    pub dirs: Dirs,
    pub host: &'static dyn Host,
    pub backend: Option<Arc<dyn Backend>>,
    pub guests: Arc<dyn Guests>,
    pub clock: Arc<dyn Clock>,
    pub progress: Arc<dyn Progress>,
    pub hooks: Arc<dyn LaunchHooks>,
    pub boot_bound: Duration,
}

pub struct Engine {
    dirs: Dirs,
    host: &'static dyn Host,
    backend: Option<Arc<dyn Backend>>,
    guests: Arc<dyn Guests>,
    clock: Arc<dyn Clock>,
    progress: Arc<dyn Progress>,
    hooks: Arc<dyn LaunchHooks>,
    boot_bound: Duration,
    registry: Registry,
    store: Lazy<LayerStore>,
    /// One exit watcher per running machine: a thread blocked until its VM exits cannot be cancelled, so one made per
    /// call or per handle would outlive them.
    exits: Mutex<Vec<(ProcessIdRecord, VmExit)>>,
}

impl Engine {
    /// The engine for this process: the real host, the directories and the configuration file from the environment,
    /// the QEMU backend and the Linux guest. Downloads go through `transport`. Downloads nothing. Reaps the leftovers
    /// of dead devvm processes (see [`Engine::reap`]), which warns on `progress` and never fails; then an unreadable
    /// `config.yaml` fails here, naming the file, so that no command can forget the check. `devvm doctor` is the one
    /// caller that must not fail on it, and goes through [`Engine::doctor_checks`].
    pub fn from_env(progress: Arc<dyn Progress>, transport: Arc<dyn Transport>) -> Result<Arc<Engine>> {
        let (engine, install) = Engine::build_from_env(progress, transport)?;
        install.config().map(|_| engine)
    }

    /// `from_env` up to the check of `config.yaml`: the engine, reaped, and the install whose configuration is still
    /// unchecked. Only the callers in this crate see it, so no engine that skipped the check leaves the crate.
    fn build_from_env(
        progress: Arc<dyn Progress>,
        transport: Arc<dyn Transport>,
    ) -> Result<(Arc<Engine>, Arc<LazyInstall>)> {
        let dirs = Dirs::from_env()?;
        let host = host::current();
        let fetcher = Arc::new(Fetcher::new(dirs.downloads(), transport, progress.clone()));
        let QemuParts {
            backend,
            guests,
            install,
        } = qemu_parts(&dirs, fetcher, host, progress.clone());
        let engine = Engine::with_parts(EngineParts {
            dirs,
            host,
            backend: Some(backend),
            guests,
            clock: Arc::new(SystemClock),
            progress,
            hooks: Arc::new(NoHooks),
            boot_bound: DEFAULT_BOOT_BOUND,
        });
        // Nothing the reap does resolves the install (see `qemu_parts`), so it reads no config: the check of it comes
        // after the reap.
        engine.reap();
        Ok((engine, install))
    }

    /// `from_env` reporting progress on stderr.
    pub fn from_env_stderr(transport: Arc<dyn Transport>) -> Result<Arc<Engine>> {
        Engine::from_env(Arc::new(StderrProgress), transport)
    }

    pub(crate) fn with_parts(parts: EngineParts) -> Arc<Engine> {
        let registry = Registry::new(&parts.dirs, parts.progress.clone(), parts.backend.clone(), parts.host);
        Arc::new(Engine {
            dirs: parts.dirs,
            host: parts.host,
            backend: parts.backend,
            guests: parts.guests,
            clock: parts.clock,
            progress: parts.progress,
            hooks: parts.hooks,
            boot_bound: parts.boot_bound,
            registry,
            store: Lazy::new(),
            exits: Mutex::default(),
        })
    }

    /// The instance called `name` (case-insensitive).
    pub fn instance(self: &Arc<Engine>, name: &str) -> Result<Instance> {
        Instance::open_in(self, name)
    }

    /// Removes instances whose owner is gone and finishes half-done removals. Never fails: what it cannot do is
    /// reported as a warning on the progress channel and retried by the next call. Until a reap, a name that a dead
    /// process half-created answers "already exists". `from_env` calls it once; call it again for a pass in a
    /// long-lived process.
    ///
    /// Three passes: `Registry::reap` (`instances/` and the files of dead machines), `build_dir::sweep` (the directories
    /// and lock files of builds that are no longer running) and the sweep of `ssh_config` entries. The last two look at
    /// what lives outside `instances/`, so `Registry::reap` does not see them: `<state>/builds` and `<state>/ssh`, where
    /// the entries of instances that no longer exist are.
    pub fn reap(&self) {
        self.registry.reap();
        build_dir::sweep(self);
        self.sweep_ssh_configs();
    }

    /// Removes the `ssh_config` entry of every instance whose record is definitely absent. A record that is valid,
    /// corrupt, from a newer devvm or unreadable keeps its entry (an unreadable one is said on progress), and the next
    /// pass looks again. `SshConfigDir::sweep` runs the predicate under the `ssh` directory's lock, so it asks the
    /// registry when it is called, never a list read before.
    fn sweep_ssh_configs(&self) {
        let live = |name: &InstanceName| {
            let dir = self.registry.root().join(name.as_str());
            match self.registry.locks().read_record(&dir) {
                Ok(RecordRead::Absent) => false,
                Ok(_) => true,
                Err(e) => {
                    self.progress
                        .message(&format!("instance {name}: cannot read its record: {}", e.message()));
                    true
                }
            }
        };
        for warning in self.ssh_configs().sweep(&live) {
            self.progress.message(&warning);
        }
    }

    /// The watcher of the VM process `machine` names, shared by every call for it through this engine. It first drops the
    /// watchers of other machines whose VM has ended, so a restart (a new process, a new record) does not keep the old
    /// one. The machine's own watcher is kept while it watches and once it saw the process exit; one that could not
    /// watch is replaced. A process that is already gone is `connection-lost`.
    pub(crate) fn vm_exit(&self, machine: &MachineRecord) -> Result<VmExit> {
        let mut exits = self.exits.lock().unwrap_or_else(PoisonError::into_inner);
        exits.retain(|(process, exit)| {
            if *process == machine.process {
                !matches!(exit.verdict(), Some(WatchEnd::Unwatched(_)))
            } else {
                !exit.has_ended()
            }
        });
        if let Some((_, exit)) = exits.iter().find(|(process, _)| *process == machine.process) {
            return Ok(exit.clone());
        }
        let Some(process) = VmProcess::attach(&machine.process)? else {
            return Err(Error::new(
                ErrorKind::ConnectionLost,
                format!("the VM process exited (serial log: {})", machine.serial_log.display()),
            ));
        };
        let exit = VmExit::watch(process)?;
        exits.push((machine.process.clone(), exit.clone()));
        Ok(exit)
    }

    /// The `ssh_config` entries of this engine's state directory.
    pub(crate) fn ssh_configs(&self) -> SshConfigDir {
        SshConfigDir::new(self.dirs.ssh()).with_progress(self.progress.clone())
    }

    /// The instances matching `filter`, by name.
    pub fn list(&self, filter: ListFilter) -> Result<Vec<InstanceInfo>> {
        self.list_with(filter, &mut |_| {})
    }

    /// `list` with `after_record` run for each instance between reading its record and its owner marker.
    pub(crate) fn list_with(
        &self,
        filter: ListFilter,
        after_record: &mut dyn FnMut(&InstanceDir),
    ) -> Result<Vec<InstanceInfo>> {
        let wanted = match &filter {
            ListFilter::All => None,
            ListFilter::CreatedIn(path) => Some((path.clone(), canonical_or_raw(path))),
        };
        let mut infos = Vec::new();
        for dir in self.registry.list()? {
            let record = match dir.record() {
                Ok(record) => record,
                // Removed since the listing.
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => {
                    self.progress
                        .message(&format!("skipping instance {}: {}", dir.name(), e.message()));
                    continue;
                }
            };
            if let Some((raw, canonical)) = &wanted
                && record.created_from != *raw
                && canonical_or_raw(&record.created_from) != *canonical
            {
                continue;
            }
            after_record(&dir);
            let ownerless = match dir.marker() {
                Ok(MarkerRead::Valid(marker)) if marker.id == *dir.id() => marker.owner == Owner::None,
                // The name belongs to another instance now: this one was removed since its record was read.
                Ok(MarkerRead::Valid(_)) => continue,
                unusable => {
                    // An instance removed since its record was read has no marker either: not worth a word.
                    if matches!(dir.record(), Err(e) if e.kind() == ErrorKind::NotFound) {
                        continue;
                    }
                    let why = match unusable {
                        Ok(MarkerRead::Corrupt(why)) => format!("unreadable ({why})"),
                        Ok(_) => "missing".to_owned(),
                        Err(e) => format!("unreadable ({})", e.message()),
                    };
                    self.progress
                        .message(&format!("skipping instance {}: its owner.json is {why}", dir.name()));
                    continue;
                }
            };
            warn_if_expired(self, dir.name(), record.expires);
            infos.push(InstanceInfo {
                name: record.name.as_str().to_owned(),
                state: self.state_of(&record.machine, dir.name().as_str()),
                guest: record.guest,
                arch: record.arch,
                created: record.created,
                created_from: record.created_from,
                expires: record.expires,
                ownerless,
            });
        }
        Ok(infos)
    }

    fn state_of(&self, machine: &Option<MachineRecord>, name: &str) -> InstanceState {
        let Some(machine) = machine else {
            return InstanceState::Stopped;
        };
        match self.backend().and_then(|backend| backend.find(machine)) {
            Ok(Some(_)) => InstanceState::Running,
            Ok(None) => InstanceState::Stopped,
            Err(e) => {
                self.progress.message(&format!(
                    "cannot tell whether instance {name} is running: {}",
                    e.message()
                ));
                InstanceState::Unknown
            }
        }
    }

    pub(crate) fn registry(&self) -> &Registry {
        &self.registry
    }

    pub(crate) fn host(&self) -> &'static dyn Host {
        self.host
    }

    pub(crate) fn guests(&self) -> &Arc<dyn Guests> {
        &self.guests
    }

    pub(crate) fn hooks(&self) -> &Arc<dyn LaunchHooks> {
        &self.hooks
    }

    /// How long a started guest gets to answer before the start fails: the failure bound of the readiness wait.
    pub(crate) fn boot_bound(&self) -> Duration {
        self.boot_bound
    }

    pub(crate) fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub(crate) fn progress(&self) -> &Arc<dyn Progress> {
        &self.progress
    }

    /// The VM backend, or `not-supported` when this build has none.
    pub(crate) fn backend(&self) -> Result<&Arc<dyn Backend>> {
        self.backend
            .as_ref()
            .ok_or_else(|| Error::new(ErrorKind::NotSupported, "this build of devvm has no VM backend"))
    }

    /// The layer store, opened on first use (opening creates its directories).
    pub(crate) fn store(&self) -> Result<&LayerStore> {
        self.store
            .get(|| LayerStore::open(&self.dirs, self.clock.clone(), self.progress.clone()))
    }
}

fn canonical_or_raw(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
