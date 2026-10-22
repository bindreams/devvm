//! Garbage collection and the record guard instance records are written under.
//!
//! Collection holds `gc.lock` exclusively, so it cannot interleave with a lookup, a publish or the writing of an
//! instance record, all of which hold it shared.
//!
//! Deadlock freedom: a build takes `<key>.build` and then, shared, `gc.lock`; collection takes `gc.lock` and then only
//! `try_lock`s `<key>.build` and `.gen` locks, never waiting on them. Nothing waits in a cycle.

use std::collections::HashSet;

use super::meta::{self, MetaRead};
use super::{ChainLease, CurrentFile, Generation, LayerLease, LayerRef, LayerStore};
use crate::recipe::hash::LayerKey;
use crate::store::lock::{FileLock, Mode};
use crate::{Error, ErrorKind, Result};

/// What is still needed by something outside the store.
pub trait Reachability {
    /// The full ancestor chain of every instance record, running or stopped.
    ///
    /// Called while the exclusive `gc.lock` is held. It must only read records and must never wait on a lock: any
    /// holder of the shared `gc.lock` that the wait depends on is itself waiting for collection to end.
    fn chains(&self) -> Result<Vec<Vec<LayerRef>>>;
}

/// The shared `gc.lock`: while it is held, collection cannot start, so a chain looked up before it is leased is not
/// collected from under the writer of an instance record.
pub struct RecordGuard {
    _lock: FileLock,
}

/// What a collection did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub deleted: Vec<LayerRef>,
    /// Unreachable generations someone still holds a lease on; left for the next collection.
    pub held: Vec<LayerRef>,
    pub staging_removed: usize,
}

impl LayerStore {
    /// Takes the shared `gc.lock` for as long as the guard lives.
    pub fn begin_record(&self) -> Result<RecordGuard> {
        let mut lock = self.gc_lock()?;
        lock.lock(Mode::Shared, "layer garbage collection", &*self.progress)?;
        Ok(RecordGuard { _lock: lock })
    }

    /// Leases `tip` and its recorded ancestors, root first. Unlike a lookup it ignores expiry: an instance may keep
    /// using a layer past it. A link that is gone or unreadable is `Err(Store)`.
    pub fn lease_chain(&self, tip: &LayerRef, _guard: &RecordGuard) -> Result<ChainLease> {
        let mut wanted = *tip;
        let mut leases: Vec<LayerLease> = Vec::new();
        let mut seen = HashSet::new();
        loop {
            let missing = || {
                Error::new(
                    ErrorKind::Store,
                    format!(
                        "layer {} generation {} is missing or unreadable",
                        wanted.key, wanted.generation
                    ),
                )
            };
            if !seen.insert(wanted) {
                return Err(missing());
            }
            let lease = self.lease(&wanted, None, false)?.ok_or_else(missing)?;
            let parent = lease.meta().parent;
            leases.push(lease);
            match parent {
                Some(parent) => wanted = parent,
                None => break,
            }
        }
        leases.reverse();
        Ok(ChainLease::new(leases))
    }

    /// Deletes every generation that is neither a key's `current`, nor in `reachable`'s chains, nor an ancestor of
    /// one of those, and that nobody holds a lease on. Removes staging directories of keys nobody is building.
    ///
    /// A `reachable` error, or a kept layer whose record or `current` file is unreadable (so its ancestors cannot be
    /// known), aborts the pass before anything is deleted. An unkept generation whose own record is unreadable is
    /// moved to quarantine, not deleted.
    pub fn gc(&self, reachable: &dyn Reachability) -> Result<GcReport> {
        let mut gc = self.gc_lock()?;
        gc.lock(
            Mode::Exclusive,
            "other devvm processes to finish with the layer store",
            &*self.progress,
        )?;
        let chains = reachable.chains()?;
        let keys = self.key_dirs()?;

        let mut keep: HashSet<LayerRef> = chains.into_iter().flatten().collect();
        for key in &keys {
            match self.read_current_file(key)? {
                CurrentFile::Absent => {}
                CurrentFile::Generation(generation) => {
                    keep.insert(LayerRef { key: *key, generation });
                }
                CurrentFile::Unreadable(path) => {
                    return Err(Error::new(
                        ErrorKind::Store,
                        format!(
                            "cannot collect: {} is unreadable, so the layer it keeps and that layer's ancestors \
                             are unknown; nothing was deleted",
                            path.display()
                        ),
                    ));
                }
            }
        }
        let mut pending: Vec<LayerRef> = keep.iter().copied().collect();
        while let Some(layer) = pending.pop() {
            let dir = self.generation_dir(&layer);
            // A kept layer that is gone has no ancestors to protect.
            if !dir.is_dir() {
                continue;
            }
            match meta::read(&dir)? {
                MetaRead::Valid(meta) => {
                    if let Some(parent) = meta.parent
                        && keep.insert(parent)
                    {
                        pending.push(parent);
                    }
                }
                MetaRead::Absent | MetaRead::Corrupt(_) => {
                    return Err(Error::new(
                        ErrorKind::Store,
                        format!(
                            "cannot collect: the record of kept layer {} generation {} ({}) is unreadable, so its \
                             ancestors are unknown; nothing was deleted",
                            layer.key,
                            layer.generation,
                            dir.display()
                        ),
                    ));
                }
            }
        }

        let mut report = GcReport::default();
        for key in &keys {
            self.collect_key(key, &keep, &mut report)?;
        }
        Ok(report)
    }

    /// The keys that have a directory in the store.
    fn key_dirs(&self) -> Result<Vec<LayerKey>> {
        let entries = std::fs::read_dir(&self.layers)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot list {}", self.layers.display()), e))?;
        let mut keys = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(ErrorKind::Io, "cannot list the layer store", e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // Only the canonical lowercase spelling is devvm's; another spelling maps to a path that may not exist.
            if let Ok(key) = name.parse::<LayerKey>()
                && key.to_string() == name
            {
                keys.push(key);
            }
        }
        Ok(keys)
    }

    fn collect_key(&self, key: &LayerKey, keep: &HashSet<LayerRef>, report: &mut GcReport) -> Result<()> {
        let key_dir = self.key_dir(key);
        let entries = std::fs::read_dir(&key_dir)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot list {}", key_dir.display()), e))?;
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(ErrorKind::Io, format!("cannot list {}", key_dir.display()), e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(generation) = name.parse::<Generation>() else {
                continue;
            };
            if generation.to_string() != name {
                continue;
            }
            let layer = LayerRef { key: *key, generation };
            if keep.contains(&layer) {
                continue;
            }
            let mut lock = self.generation_lock(&layer)?;
            if !lock.try_lock(Mode::Exclusive)? {
                report.held.push(layer);
                continue;
            }
            if let MetaRead::Corrupt(why) = meta::read(&entry.path())? {
                self.progress.message(&format!(
                    "layer record {} is unreadable ({why}); moving the layer to quarantine instead of deleting it",
                    entry.path().display()
                ));
                self.quarantine_record(&layer, true)?;
                continue;
            }
            match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => {
                    report.deleted.push(layer);
                    drop(lock);
                    // Best effort: a lock file left behind is reused by the next holder of that generation.
                    let _ = std::fs::remove_file(self.lock_path(&format!("{key}-{generation}.gen")));
                }
                Err(e) => {
                    self.progress.message(&format!(
                        "cannot delete layer {}: {e}; leaving it for the next collection",
                        entry.path().display()
                    ));
                    report.held.push(layer);
                }
            }
        }

        // Leftovers are dead exactly when no builder holds the key's build lock.
        let mut build = self.build_lock(key)?;
        if !build.try_lock(Mode::Exclusive)? {
            return Ok(());
        }
        report.staging_removed += self.sweep_leftovers(&key_dir);
        // Fails while the directory still holds anything, which is the point.
        let _ = std::fs::remove_dir(&key_dir);
        Ok(())
    }
}
