//! The layer store: content-addressed layers with generations, leases and atomic publish.
//!
//! On disk, under the layers directory (32-hex names keep Windows paths short):
//!
//! - `<key>/<generation>/`: one published generation, holding `meta.json` and the backend's files;
//! - `<key>/current`: the generation a lookup uses;
//! - `<key>/.staging-<random>/`: a build in progress, a sibling of the generation directories (same depth, so relative
//!   backing-file paths stay valid after the publishing rename);
//! - `.locks/`: `gc.lock`, `<key>.build` and `<key>-<generation>.gen`, outside every directory that gets deleted.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::exit::Progress;
use crate::recipe::hash::LayerKey;
use crate::store::lock::{FileLock, Mode};
use crate::{Dirs, Error, ErrorKind, Result};

mod gc;
mod lease;
mod meta;

pub use gc::{GcReport, Reachability, RecordGuard};
pub use lease::{ChainLease, LayerLease};
pub use meta::{Generation, LayerMeta, LayerRef};

use meta::MetaRead;

#[cfg(test)]
#[path = "layers_tests.rs"]
pub(crate) mod layers_tests;

#[cfg(test)]
#[path = "layers/cross_tests.rs"]
pub(crate) mod cross_tests;

#[cfg(test)]
#[path = "layers/gc_tests.rs"]
pub(crate) mod gc_tests;

#[cfg(test)]
#[path = "layers/corrupt_tests.rs"]
mod corrupt_tests;

/// A lookup within this long of a layer's expiry is a miss, so a layer is never handed out just before it expires.
pub const EXPIRY_MARGIN: Duration = Duration::from_secs(10 * 24 * 3600);

// Public types ========================================================================================================

/// Wall-clock time, injected so expiry is testable.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Where generation ids come from. Production draws random bits; tests observe when they are drawn.
pub(crate) trait IdSource: Send + Sync {
    fn generation(&self) -> Generation;
}

struct RandomIds;

impl IdSource for RandomIds {
    fn generation(&self) -> Generation {
        Generation::random()
    }
}

/// A directory a backend writes a layer into (a qcow2 plus optional UEFI vars; later other formats).
#[derive(Clone, Debug)]
pub struct LayerDir(PathBuf);

impl LayerDir {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// The directory a builder fills.
pub struct Staging<'a> {
    pub dir: &'a LayerDir,
    /// The generation this build will publish as.
    pub generation: Generation,
    /// The tip of the parent chain, if any.
    pub parent: Option<&'a LayerLease>,
}

/// What a builder reports.
#[derive(Clone, Copy, Debug, Default)]
pub struct Built {
    pub expires: Option<SystemTime>,
}

impl Built {
    /// A `LayerMeta` returned by a `BaseBuilder` contributes only its expiry: the store owns key, generation, parent
    /// and creation time.
    pub fn from_meta(meta: &LayerMeta) -> Built {
        Built { expires: meta.expires }
    }
}

/// What to build when a lookup misses.
pub struct BuildRequest {
    pub key: LayerKey,
    /// Names the base in an `expired-base` error, e.g. `ISO C:\isos\win11.iso`.
    pub source_label: String,
}

// The store ===========================================================================================================

pub struct LayerStore {
    layers: PathBuf,
    quarantine: PathBuf,
    clock: Arc<dyn Clock>,
    progress: Arc<dyn Progress>,
    ids: Arc<dyn IdSource>,
}

impl LayerStore {
    pub fn open(dirs: &Dirs, clock: Arc<dyn Clock>, progress: Arc<dyn Progress>) -> Result<LayerStore> {
        LayerStore::open_with_ids(dirs, clock, progress, Arc::new(RandomIds))
    }

    pub(crate) fn open_with_ids(
        dirs: &Dirs,
        clock: Arc<dyn Clock>,
        progress: Arc<dyn Progress>,
        ids: Arc<dyn IdSource>,
    ) -> Result<LayerStore> {
        let layers = dirs.layers();
        std::fs::create_dir_all(layers.join(".locks")).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot create the layer store {}", layers.display()),
                e,
            )
        })?;
        Ok(LayerStore {
            layers,
            quarantine: dirs.quarantine(),
            clock,
            progress,
            ids,
        })
    }

    // Paths -----------------------------------------------------------------------------------------------------------

    // Quarantine ------------------------------------------------------------------------------------------------------

    /// A fresh, not yet existing path under the quarantine root; its parent exists.
    fn quarantine_path(&self, label: &str) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.quarantine)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", self.quarantine.display()), e))?;
        Ok(self.quarantine.join(format!("{label}-{}", Generation::random())))
    }

    /// Moves an unreadable `current` file to quarantine. Never deletes it.
    fn quarantine_current(&self, key: &LayerKey, current: &Path) -> Result<()> {
        let dir = self.quarantine_path(&format!("{key}-current"))?;
        std::fs::create_dir(&dir)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", dir.display()), e))?;
        let target = dir.join("current");
        std::fs::rename(current, &target).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot move {} to {}", current.display(), target.display()),
                e,
            )
        })?;
        self.progress.message(&format!(
            "moved the unreadable {} to {}",
            current.display(),
            target.display()
        ));
        Ok(())
    }

    /// Moves a generation whose record is unreadable to quarantine, unless someone holds a lease on it: then the
    /// directory stays where it is and only a copy of its record is kept. Never deletes anything.
    fn quarantine_generation(&self, layer: &LayerRef) -> Result<()> {
        let mut lock = self.generation_lock(layer)?;
        let exclusive = lock.try_lock(Mode::Exclusive)?;
        self.quarantine_record(layer, exclusive)
    }

    /// `quarantine_generation` for a caller that already tried the generation's exclusive lock: `exclusive` says
    /// whether it holds it.
    pub(super) fn quarantine_record(&self, layer: &LayerRef, exclusive: bool) -> Result<()> {
        let source = self.generation_dir(layer);
        let dir = self.quarantine_path(&format!("{}-{}", layer.key, layer.generation))?;
        if exclusive {
            match std::fs::rename(&source, &dir) {
                Ok(()) => {
                    self.progress.message(&format!(
                        "moved the unreadable layer {} to {}",
                        source.display(),
                        dir.display()
                    ));
                    return Ok(());
                }
                Err(e) => self.progress.message(&format!(
                    "cannot move {} to quarantine: {e}; keeping a copy of its record",
                    source.display()
                )),
            }
        } else {
            self.progress.message(&format!(
                "layer {} is in use; leaving it in place and keeping a copy of its record in {}",
                source.display(),
                dir.display()
            ));
        }
        std::fs::create_dir(&dir)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", dir.display()), e))?;
        std::fs::copy(source.join("meta.json"), dir.join("meta.json"))
            .map(|_| ())
            .map_err(|e| {
                Error::io(
                    ErrorKind::Io,
                    format!("cannot copy the record of {}", source.display()),
                    e,
                )
            })
    }

    pub(crate) fn key_dir(&self, key: &LayerKey) -> PathBuf {
        self.layers.join(key.to_string())
    }

    pub(crate) fn generation_dir(&self, layer: &LayerRef) -> PathBuf {
        self.key_dir(&layer.key).join(layer.generation.to_string())
    }

    fn lock_path(&self, name: &str) -> PathBuf {
        self.layers.join(".locks").join(name)
    }

    fn gc_lock(&self) -> Result<FileLock> {
        FileLock::open(&self.lock_path("gc.lock"))
    }

    fn build_lock(&self, key: &LayerKey) -> Result<FileLock> {
        FileLock::open(&self.lock_path(&format!("{key}.build")))
    }

    pub(crate) fn generation_lock(&self, layer: &LayerRef) -> Result<FileLock> {
        FileLock::open(&self.lock_path(&format!("{}-{}.gen", layer.key, layer.generation)))
    }

    // Lookup ----------------------------------------------------------------------------------------------------------

    /// Leases the chain published under `key`, or `None` on a miss.
    pub fn lookup(&self, key: &LayerKey) -> Result<Option<ChainLease>> {
        self.lookup_with(key, false, None)
    }

    /// `lookup`; with `repair` it also moves `key`'s own unreadable records to quarantine. Only a caller holding the
    /// key's build lock may repair. `now` is the time to judge expiry by; `None` reads the clock after the locks are
    /// held, so a waiter that gets the build lock never judges by a stale time.
    fn lookup_with(&self, key: &LayerKey, repair: bool, now: Option<SystemTime>) -> Result<Option<ChainLease>> {
        let mut gc = self.gc_lock()?;
        gc.lock(Mode::Shared, "layer garbage collection", &*self.progress)?;
        let now = now.unwrap_or_else(|| self.clock.now());
        self.lookup_locked(key, repair, now)
    }

    /// `lookup_with` with the shared `gc.lock` already held by the caller.
    fn lookup_locked(&self, key: &LayerKey, repair: bool, now: SystemTime) -> Result<Option<ChainLease>> {
        let Some(generation) = self.read_current(key, repair)? else {
            return Ok(None);
        };
        let mut wanted = LayerRef { key: *key, generation };
        let mut leases = Vec::new();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(wanted) {
                self.progress.message(&format!(
                    "layer record of {} refers back to itself; ignoring it",
                    wanted.key
                ));
                return Ok(None);
            }
            let Some(lease) = self.lease(&wanted, Some(now), repair && wanted.key == *key)? else {
                return Ok(None);
            };
            let parent = lease.meta().parent;
            leases.push(lease);
            match parent {
                Some(parent) => wanted = parent,
                None => break,
            }
        }
        leases.reverse();
        Ok(Some(ChainLease::new(leases)))
    }

    /// Takes the shared lock on one generation and reads its record; `None` when it is missing or unusable, or (when
    /// `now` is given) within the expiry margin.
    fn lease(&self, layer: &LayerRef, now: Option<SystemTime>, repair: bool) -> Result<Option<LayerLease>> {
        let mut lock = self.generation_lock(layer)?;
        lock.lock(Mode::Shared, "a layer being replaced", &*self.progress)?;
        let dir = self.generation_dir(layer);
        match meta::read(&dir)? {
            MetaRead::Absent => Ok(None),
            MetaRead::Corrupt(why) => {
                self.progress.message(&format!(
                    "layer record {} is unreadable ({why}); treating it as missing",
                    dir.display()
                ));
                if repair {
                    drop(lock);
                    self.quarantine_generation(layer)?;
                }
                Ok(None)
            }
            MetaRead::Valid(meta) if meta.key != layer.key || meta.generation != layer.generation => {
                self.progress.message(&format!(
                    "layer record {} names another layer; treating it as missing",
                    dir.display()
                ));
                Ok(None)
            }
            MetaRead::Valid(meta) if now.is_some_and(|now| meta.expires.is_some_and(|e| now + EXPIRY_MARGIN >= e)) => {
                Ok(None)
            }
            MetaRead::Valid(meta) => Ok(Some(LayerLease::new(meta, LayerDir(dir), lock))),
        }
    }

    /// The generation `<key>/current` names; `None` when there is none or it is unusable.
    fn read_current(&self, key: &LayerKey, repair: bool) -> Result<Option<Generation>> {
        match self.read_current_file(key)? {
            CurrentFile::Absent => Ok(None),
            CurrentFile::Generation(generation) => Ok(Some(generation)),
            CurrentFile::Unreadable(path) => {
                self.progress.message(&format!(
                    "{} is unreadable; treating the layer as missing",
                    path.display()
                ));
                if repair {
                    self.quarantine_current(key, &path)?;
                }
                Ok(None)
            }
        }
    }

    pub(super) fn read_current_file(&self, key: &LayerKey) -> Result<CurrentFile> {
        let path = self.key_dir(key).join("current");
        let text = match std::fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(CurrentFile::Absent),
            Err(e) => return Err(Error::io(ErrorKind::Io, format!("cannot read {}", path.display()), e)),
        };
        Ok(match text.trim().parse::<Generation>() {
            Ok(generation) => CurrentFile::Generation(generation),
            Err(_) => CurrentFile::Unreadable(path),
        })
    }

    // Build -----------------------------------------------------------------------------------------------------------

    /// Returns the layer for `req.key`, running `builder` into a fresh staging directory when there is none.
    ///
    /// Concurrent builds of one key run `builder` once: the build lock serializes them and the waiter looks again
    /// (with a fresh clock read) once it has the lock. A builder error or panic removes the staging directory and
    /// publishes nothing.
    pub fn build<F>(&self, req: &BuildRequest, parent: Option<&ChainLease>, builder: F) -> Result<ChainLease>
    where
        F: FnOnce(&Staging<'_>) -> Result<Built>,
    {
        if let Some(hit) = self.lookup(&req.key)? {
            return Ok(hit);
        }
        let mut build_lock = self.build_lock(&req.key)?;
        build_lock.lock(
            Mode::Exclusive,
            &format!("another devvm process building layer {}", req.key),
            &*self.progress,
        )?;
        if let Some(hit) = self.lookup_with(&req.key, true, None)? {
            return Ok(hit);
        }
        if let Some(parent) = parent {
            self.check_parent(parent)?;
        }

        let key_dir = self.key_dir(&req.key);
        std::fs::create_dir_all(&key_dir)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", key_dir.display()), e))?;
        self.sweep_leftovers(&key_dir);

        let generation = self.ids.generation();
        let staging_path = key_dir.join(format!(".staging-{}", Generation::random()));
        std::fs::create_dir(&staging_path)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", staging_path.display()), e))?;
        let mut cleanup = RemoveOnDrop::new(staging_path.clone(), self.progress.clone());

        let staging_dir = LayerDir(staging_path.clone());
        let staging = Staging {
            dir: &staging_dir,
            generation,
            parent: parent.map(ChainLease::tip),
        };
        let built = builder(&staging)?;

        let parent_expiry = parent.and_then(|parent| parent.tip().meta().expires);
        let expires = match (built.expires, parent_expiry) {
            (Some(own), Some(inherited)) => Some(own.min(inherited)),
            (own, inherited) => own.or(inherited),
        };
        let created = self.clock.now();
        if let Some(expires) = expires
            && created + EXPIRY_MARGIN >= expires
        {
            return Err(Error::new(
                ErrorKind::ExpiredBase,
                format!(
                    "the base for layer {} expires on {}, within {} days: {}",
                    req.key,
                    date_text(expires),
                    EXPIRY_MARGIN.as_secs() / (24 * 3600),
                    req.source_label
                ),
            ));
        }
        let meta = LayerMeta {
            key: req.key,
            generation,
            parent: parent.map(|parent| parent.tip().layer_ref()),
            created,
            expires,
        };
        self.write_meta(&staging_path, &meta)?;
        self.publish(&req.key, &staging_path, &generation, &mut cleanup)?;

        // Judged at the stamped time: the published layer is valid by construction, however long publishing took.
        match self.lookup_with(&req.key, false, Some(created))? {
            Some(lease) => Ok(lease),
            None => Err(Error::new(
                ErrorKind::Internal,
                format!("layer {} was published but cannot be read back", req.key),
            )),
        }
    }

    fn check_parent(&self, parent: &ChainLease) -> Result<()> {
        for layer in parent.refs() {
            let dir = self.generation_dir(&layer);
            if !dir.is_dir() {
                return Err(Error::new(
                    ErrorKind::Store,
                    format!(
                        "parent layer {} generation {} no longer exists ({} is gone)",
                        layer.key,
                        layer.generation,
                        dir.display()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Removes what an interrupted build left in `key_dir`: staging directories and `current.tmp-*` files. The caller
    /// holds the key's build lock, and a live builder holds it for its whole build, so every such entry is dead.
    /// Returns how many staging directories it removed.
    pub(super) fn sweep_leftovers(&self, key_dir: &Path) -> usize {
        let entries = match std::fs::read_dir(key_dir) {
            Ok(entries) => entries,
            Err(e) => {
                self.progress
                    .message(&format!("cannot list {} for leftovers: {e}", key_dir.display()));
                return 0;
            }
        };
        let mut staging_removed = 0;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    self.progress
                        .message(&format!("cannot list an entry of {}: {e}", key_dir.display()));
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let result = if name.starts_with(".staging-") {
                std::fs::remove_dir_all(entry.path()).inspect(|()| staging_removed += 1)
            } else if name.starts_with("current.tmp-") {
                std::fs::remove_file(entry.path())
            } else {
                continue;
            };
            if let Err(e) = result {
                self.progress
                    .message(&format!("cannot remove leftover {}: {e}", entry.path().display()));
            }
        }
        staging_removed
    }

    fn write_meta(&self, staging: &Path, meta: &LayerMeta) -> Result<()> {
        let path = staging.join("meta.json");
        write_durable(&path, meta.to_json()?.as_bytes())
    }

    /// Renames staging to the generation directory and replaces `current`, under the shared `gc.lock`. Disarms
    /// `cleanup` once the rename has moved the staging directory away.
    fn publish(
        &self,
        key: &LayerKey,
        staging: &Path,
        generation: &Generation,
        cleanup: &mut RemoveOnDrop,
    ) -> Result<()> {
        let mut gc = self.gc_lock()?;
        gc.lock(Mode::Shared, "layer garbage collection", &*self.progress)?;
        let key_dir = self.key_dir(key);
        let target = key_dir.join(generation.to_string());
        std::fs::rename(staging, &target).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot publish {} as {}", staging.display(), target.display()),
                e,
            )
        })?;
        cleanup.disarm();
        let temp = key_dir.join(format!("current.tmp-{}", Generation::random()));
        write_durable(&temp, generation.to_string().as_bytes())?;
        let current = key_dir.join("current");
        std::fs::rename(&temp, &current)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot replace {}", current.display()), e))
    }
}

// Helpers =============================================================================================================

/// `YYYY-MM-DD` in UTC.
fn date_text(time: SystemTime) -> String {
    match jiff::Timestamp::try_from(time) {
        Ok(timestamp) => timestamp.to_zoned(jiff::tz::TimeZone::UTC).date().to_string(),
        Err(_) => "an unrepresentable date".to_owned(),
    }
}

/// What `<key>/current` holds.
pub(super) enum CurrentFile {
    Absent,
    Generation(Generation),
    /// Present but not a generation id; the file's path.
    Unreadable(PathBuf),
}

/// Writes `bytes` to a new file and flushes it to disk.
fn write_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let fail = |e| Error::io(ErrorKind::Io, format!("cannot write {}", path.display()), e);
    let mut file = std::fs::File::create(path).map_err(fail)?;
    file.write_all(bytes).map_err(fail)?;
    file.sync_all().map_err(fail)
}

/// Removes a staging directory on drop (error return or panic) unless disarmed.
struct RemoveOnDrop {
    path: PathBuf,
    progress: Arc<dyn Progress>,
    armed: bool,
}

impl RemoveOnDrop {
    fn new(path: PathBuf, progress: Arc<dyn Progress>) -> RemoveOnDrop {
        RemoveOnDrop {
            path,
            progress,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if self.armed
            && let Err(e) = std::fs::remove_dir_all(&self.path)
        {
            self.progress
                .message(&format!("cannot remove staging directory {}: {e}", self.path.display()));
        }
    }
}
