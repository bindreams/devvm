//! Creating, opening and listing instances: directories named by instance name under `<state>/instances`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use crate::backend::Storage;
use crate::exit::Progress;
use crate::instance::dir::InstanceDir;
use crate::instance::locks::Locks;
use crate::instance::name::InstanceName;
use crate::instance::record::{InstanceId, InstanceRecord, Marker, Owner, RecordRead, write_marker};
use crate::store::layers::{ChainLease, RecordGuard};
use crate::store::lock::FileLock;
use crate::{Arch, Dirs, Error, ErrorKind, GuestKind, Network, Result};

#[cfg(test)]
#[path = "registry_tests.rs"]
pub(crate) mod registry_tests;

#[cfg(all(test, windows))]
#[path = "registry_windows_tests.rs"]
mod registry_windows_tests;

/// The instances under one state directory.
pub(crate) struct Registry {
    root: PathBuf,
    locks: Arc<Locks>,
    progress: Arc<dyn Progress>,
}

/// What `create` needs to make an instance.
pub(crate) struct NewInstance<'a> {
    /// `None` generates a free name.
    pub name: Option<InstanceName>,
    /// The layers the instance sits on; their generations stay alive through the instance's record.
    pub chain: &'a ChainLease,
    pub guest: GuestKind,
    pub arch: Arch,
    pub disk_size: u64,
    pub cpus: u32,
    pub memory: u64,
    pub network: Network,
    /// The directory the command was run from.
    pub created_from: PathBuf,
    pub created: SystemTime,
    pub storage: &'a dyn Storage,
}

/// A freshly created instance whose owner is this process: it holds the owner lock until it is dropped or made
/// ownerless.
pub(crate) struct Owned {
    dir: InstanceDir,
    owner: FileLock,
}

impl Registry {
    /// Touches nothing on disk.
    pub(crate) fn new(dirs: &Dirs, progress: Arc<dyn Progress>) -> Registry {
        Registry {
            root: dirs.instances(),
            locks: Arc::new(Locks::new(dirs.instance_locks())),
            progress,
        }
    }

    pub(crate) fn locks(&self) -> &Arc<Locks> {
        &self.locks
    }

    /// `<state>/instances`.
    pub(crate) fn root(&self) -> &std::path::Path {
        &self.root
    }

    pub(crate) fn progress(&self) -> &Arc<dyn Progress> {
        &self.progress
    }

    fn handle(&self, id: InstanceId, name: InstanceName) -> InstanceDir {
        let path = self.root.join(name.as_str());
        InstanceDir::new(id, name, path, self.locks.clone(), self.progress.clone())
    }

    /// The instance called `name`. `NotFound` if there is none.
    pub(crate) fn open(&self, name: &InstanceName) -> Result<InstanceDir> {
        let path = self.root.join(name.as_str());
        match InstanceRecord::read(&path)? {
            RecordRead::Valid(record) => Ok(self.handle(record.id, record.name)),
            RecordRead::Absent => Err(Error::new(ErrorKind::NotFound, format!("no instance called {name}"))),
            RecordRead::Corrupt(why) => Err(Error::new(
                ErrorKind::Store,
                format!(
                    "the record of instance {name} is unreadable ({why}): {}",
                    path.join("record.json").display()
                ),
            )),
        }
    }

    /// Every placed instance, by name. An instance whose record cannot be read is skipped with a warning; the reaper
    /// owns quarantine.
    pub(crate) fn list(&self) -> Result<Vec<InstanceDir>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(Error::io(
                    ErrorKind::Io,
                    format!("cannot list {}", self.root.display()),
                    e,
                ));
            }
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|e| Error::io(ErrorKind::Io, format!("cannot list {}", self.root.display()), e))?;
            let file_name = entry.file_name();
            let Some(text) = file_name.to_str() else { continue };
            if text.starts_with('.') || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let Ok(name) = InstanceName::parse(text) else {
                self.progress
                    .message(&format!("ignoring {}: not an instance name", entry.path().display()));
                continue;
            };
            match self.open(&name) {
                Ok(dir) => found.push(dir),
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => self
                    .progress
                    .message(&format!("skipping instance {name}: {}", e.message())),
            }
        }
        found.sort_by(|a, b| a.name().as_str().cmp(b.name().as_str()));
        Ok(found)
    }

    /// Creates an instance owned by this process. `_guard` proves the chain was leased and the record is written under
    /// the shared `gc.lock`, so garbage collection cannot take the chain in between.
    pub(crate) fn create(&self, new: NewInstance<'_>, guard: &RecordGuard) -> Result<Owned> {
        self.create_with(new, guard, &mut || {})
    }

    /// `create` with `before_rename` run once the temporary directory is complete and before it takes its name.
    ///
    /// Order: take the owner lock first (its file is keyed by the new id, outside the directory), build the instance
    /// under `.new-<id>`, then rename it into place. The reaper treats `.new-<id>` as reapable only after taking the
    /// same owner lock, so it cannot remove a directory that is being built.
    pub(crate) fn create_with(
        &self,
        new: NewInstance<'_>,
        _guard: &RecordGuard,
        before_rename: &mut dyn FnMut(),
    ) -> Result<Owned> {
        let id = InstanceId::random();
        let owner = self.locks.acquire_owner_new(&id)?;
        let temp = self.root.join(format!(".new-{id}"));
        match self.build_and_place(id, &new, &temp, before_rename) {
            Ok(dir) => Ok(Owned { dir, owner }),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&temp);
                self.locks.remove(&id);
                drop(owner);
                Err(e)
            }
        }
    }

    fn build_and_place(
        &self,
        id: InstanceId,
        new: &NewInstance<'_>,
        temp: &std::path::Path,
        before_rename: &mut dyn FnMut(),
    ) -> Result<InstanceDir> {
        let io = |what: &str, path: &std::path::Path, e| {
            Error::io(ErrorKind::Io, format!("cannot {what} {}", path.display()), e)
        };
        std::fs::create_dir_all(&self.root).map_err(|e| io("create", &self.root, e))?;
        std::fs::create_dir(temp).map_err(|e| io("create", temp, e))?;
        new.storage
            .create_instance_disk(new.chain.tip().dir(), &temp.join("disk.img"), new.disk_size)?;
        write_marker(
            temp,
            &Marker {
                owner: Owner::Process,
                id,
            },
        )?;

        let mut name = new.name.clone().unwrap_or_else(InstanceName::generate);
        loop {
            InstanceRecord {
                id,
                name: name.clone(),
                created: new.created,
                created_from: new.created_from.clone(),
                guest: new.guest,
                arch: new.arch,
                chain: new.chain.refs(),
                expires: new.chain.tip().meta().expires,
                cpus: new.cpus,
                memory: new.memory,
                network: new.network,
                machine: None,
            }
            .write(temp)?;
            before_rename();
            let target = self.root.join(name.as_str());
            match std::fs::rename(temp, &target) {
                Ok(()) => return Ok(self.handle(id, name)),
                Err(e) if target.exists() => {
                    if new.name.is_some() {
                        return Err(Error::new(
                            ErrorKind::AlreadyExists,
                            format!("an instance called {name} already exists"),
                        )
                        .with_source(e));
                    }
                    name = InstanceName::generate();
                }
                Err(e) => return Err(io("rename", temp, e)),
            }
        }
    }
}

impl Owned {
    pub(crate) fn dir(&self) -> &InstanceDir {
        &self.dir
    }

    /// Hands the instance over to nobody: it then stays until `rm`.
    ///
    /// Runs `disarm` first (the VM's containment, after its record is written and it was resumed), then flips the owner
    /// marker, all while the owner lock is held; the lock is released last. If `disarm` fails nothing is flipped and
    /// the lock is released with the instance still owned, so the reaper takes it. Flipping first would leave an
    /// ownerless instance whose VM a dying owner's job then kills.
    pub(crate) fn make_ownerless(self, disarm: &mut dyn FnMut() -> Result<()>) -> Result<InstanceDir> {
        let Owned { dir, owner } = self;
        disarm()?;
        write_marker(
            dir.path(),
            &Marker {
                owner: Owner::None,
                id: *dir.id(),
            },
        )?;
        drop(owner);
        Ok(dir)
    }
}
