//! Creating, opening and listing instances: directories named by instance name under `<state>/instances`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use crate::backend::{Backend, Storage};
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
    quarantine: PathBuf,
    locks: Arc<Locks>,
    progress: Arc<dyn Progress>,
    /// How the reaper finds and kills the machines of dead owners; `None` in a build without a VM backend.
    backend: Option<Arc<dyn Backend>>,
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

/// Where a test can step into `create`; the defaults do nothing and generate real names.
pub(crate) struct CreateSeams<'a> {
    /// Once the temporary directory is complete and before it takes its name.
    pub before_rename: Box<dyn FnMut() + 'a>,
    /// Just before the placement waits for the readers of instance files.
    pub before_wait: Box<dyn FnMut() + 'a>,
    /// After the placement was tried, whatever came of it.
    pub after_rename: Box<dyn FnMut() + 'a>,
    /// The next name to try, when the caller gave none or the name was taken.
    pub generate: Box<dyn FnMut() -> InstanceName + 'a>,
}

impl CreateSeams<'_> {
    pub(crate) fn none<'a>() -> CreateSeams<'a> {
        CreateSeams {
            before_rename: Box::new(|| {}),
            before_wait: Box::new(|| {}),
            after_rename: Box::new(|| {}),
            generate: Box::new(InstanceName::generate),
        }
    }
}

/// A freshly created instance whose owner is this process: it holds the owner lock until it is dropped or made
/// ownerless.
pub(crate) struct Owned {
    dir: InstanceDir,
    owner: FileLock,
}

impl Registry {
    /// Touches nothing on disk.
    pub(crate) fn new(dirs: &Dirs, progress: Arc<dyn Progress>, backend: Option<Arc<dyn Backend>>) -> Registry {
        Registry {
            root: dirs.instances(),
            quarantine: dirs.instance_quarantine(),
            locks: Arc::new(Locks::new(dirs.instance_locks())),
            progress,
            backend,
        }
    }

    pub(super) fn backend(&self) -> Option<&Arc<dyn Backend>> {
        self.backend.as_ref()
    }

    pub(crate) fn locks(&self) -> &Arc<Locks> {
        &self.locks
    }

    /// `<state>/instances`.
    pub(crate) fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// `<state>/quarantine`.
    pub(super) fn quarantine_root(&self) -> &std::path::Path {
        &self.quarantine
    }

    pub(crate) fn progress(&self) -> &Arc<dyn Progress> {
        &self.progress
    }

    pub(super) fn handle(&self, id: InstanceId, name: InstanceName) -> InstanceDir {
        let path = self.root.join(name.as_str());
        InstanceDir::new(id, name, path, self.locks.clone(), self.progress.clone())
    }

    /// The instance called `name`. `NotFound` if there is none.
    pub(crate) fn open(&self, name: &InstanceName) -> Result<InstanceDir> {
        let path = self.root.join(name.as_str());
        match self.locks.read_record(&path)? {
            RecordRead::Valid(record) if record.name == *name => Ok(self.handle(record.id, record.name)),
            // A moved or copied directory: its record is for another name. The reaper moves it to quarantine.
            RecordRead::Valid(record) => Err(Error::new(
                ErrorKind::Store,
                format!(
                    "the record in the directory of instance {name} is for an instance called {} ({}); the next devvm command \
                     moves the directory to quarantine",
                    record.name,
                    path.join("record.json").display()
                ),
            )),
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
            let Some(name) = self.listed_name(&entry.file_name(), &entry.path(), entry.file_type()) else {
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

    /// The instance name of the directory entry `path`, if it is one. `kind` is the result of asking for its type.
    /// Anything that is not a candidate is skipped; an entry whose type cannot be read, or whose name is not an
    /// instance name, is skipped with a warning, since an instance may be missing from the listing.
    pub(super) fn listed_name(
        &self,
        file_name: &std::ffi::OsStr,
        path: &std::path::Path,
        kind: std::io::Result<std::fs::FileType>,
    ) -> Option<InstanceName> {
        let text = file_name.to_str()?;
        if text.starts_with('.') {
            return None;
        }
        match kind {
            Ok(kind) if !kind.is_dir() => return None,
            Ok(_) => {}
            Err(e) => {
                self.progress
                    .message(&format!("skipping {}: cannot tell what it is ({e})", path.display()));
                return None;
            }
        }
        match InstanceName::parse(text) {
            Ok(name) => Some(name),
            Err(_) => {
                self.progress
                    .message(&format!("ignoring {}: not an instance name", path.display()));
                None
            }
        }
    }

    /// Creates an instance owned by this process. `_guard` proves the chain was leased and the record is written under
    /// the shared `gc.lock`, so garbage collection cannot take the chain in between.
    pub(crate) fn create(&self, new: NewInstance<'_>, guard: &RecordGuard) -> Result<Owned> {
        self.create_with(new, guard, &mut || {})
    }

    /// `create` with `before_rename` run once the temporary directory is complete and before it takes its name.
    pub(crate) fn create_with(
        &self,
        new: NewInstance<'_>,
        guard: &RecordGuard,
        before_rename: &mut dyn FnMut(),
    ) -> Result<Owned> {
        self.create_with_seams(
            new,
            guard,
            &mut CreateSeams {
                before_rename: Box::new(before_rename),
                ..CreateSeams::none()
            },
        )
    }

    /// `create` with the test seams of [`CreateSeams`].
    ///
    /// Order: take the owner lock first (its file is keyed by the new id, outside the directory), build the instance
    /// under `.new-<id>`, then rename it into place. The reaper treats `.new-<id>` as reapable only after taking the
    /// same owner lock, so it cannot remove a directory that is being built.
    pub(crate) fn create_with_seams(
        &self,
        new: NewInstance<'_>,
        _guard: &RecordGuard,
        seams: &mut CreateSeams<'_>,
    ) -> Result<Owned> {
        let id = InstanceId::random();
        let owner = self.locks.acquire_owner_new(&id)?;
        let temp = self.root.join(format!(".new-{id}"));
        match self.build_and_place(id, &new, &temp, seams) {
            Ok(dir) => Ok(Owned { dir, owner }),
            Err(e) => {
                if let Err(left) = std::fs::remove_dir_all(&temp)
                    && left.kind() != std::io::ErrorKind::NotFound
                {
                    self.progress.message(&format!(
                        "the half-built instance {} could not be deleted ({left}); the next devvm \
                         command deletes it",
                        temp.display()
                    ));
                }
                // Already failing with the creation's error; a lock file left over is swept by the next reaper pass.
                let _ = self.locks.remove(&id);
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
        seams: &mut CreateSeams<'_>,
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

        let mut name = new.name.clone().unwrap_or_else(&mut seams.generate);
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
                launches: 0,
                machine: None,
            }
            .write(temp)?;
            (seams.before_rename)();
            let target = self.root.join(name.as_str());
            // Placed while no one reads inside any instance directory, like every other rename of one.
            let placed = self
                .locks
                .excluding_readers(&mut seams.before_wait, || place(temp, &target))?;
            (seams.after_rename)();
            match placed {
                Placement::Placed => return Ok(self.handle(id, name)),
                Placement::Taken(e) => {
                    if new.name.is_some() {
                        return Err(Error::new(
                            ErrorKind::AlreadyExists,
                            format!("an instance called {name} already exists"),
                        )
                        .with_source(e));
                    }
                    name = (seams.generate)();
                }
                Placement::Failed(e) => return Err(io("rename", temp, e)),
            }
        }
    }
}

/// How the rename of a new instance to its name went.
enum Placement {
    Placed,
    /// The name belongs to another instance.
    Taken(std::io::Error),
    Failed(std::io::Error),
}

/// Renames `temp` to `target` and, when that fails, tells a taken name from any other failure. Runs while no one
/// renames an instance directory, so no `rm` can free the name between the failure and the look.
fn place(temp: &std::path::Path, target: &std::path::Path) -> Placement {
    match std::fs::rename(temp, target) {
        Ok(()) => Placement::Placed,
        Err(e) if target.exists() => Placement::Taken(e),
        Err(e) => Placement::Failed(e),
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
        self.make_ownerless_with(disarm, &mut || {})
    }

    /// `make_ownerless` with `during_write` run just before the marker is written.
    pub(crate) fn make_ownerless_with(
        self,
        disarm: &mut dyn FnMut() -> Result<()>,
        during_write: &mut dyn FnMut(),
    ) -> Result<InstanceDir> {
        let Owned { dir, owner } = self;
        disarm()?;
        // Written while no one can rename the directory away, and only into this instance's own directory.
        dir.write_marker(
            &Marker {
                owner: Owner::None,
                id: *dir.id(),
            },
            during_write,
        )?;
        drop(owner);
        Ok(dir)
    }
}
