//! Garbage collection and the record guard instance records are written under.
//!
//! Collection holds `gc.lock` exclusively, so it cannot interleave with a lookup, a publish or the writing of an
//! instance record, all of which hold it shared.
//!
//! Deadlock freedom: a build takes `<key>.build` and then, shared, `gc.lock`; collection takes `gc.lock` and then only
//! `try_lock`s `<key>.build` and `.gen` locks, never waiting on them. Nothing waits in a cycle.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::meta::{self, MetaRead};
use super::{ChainLease, CurrentFile, Generation, LayerLease, LayerRef, LayerStore};
use crate::host::{look_failed, symlink_metadata_if_present};
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
        lock.lock(Mode::Shared, "layer garbage collection", &mut |m| {
            self.progress.message(m)
        })?;
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
    /// A `reachable` error, a failed look at a kept generation's directory, a kept layer whose record is missing,
    /// unreadable or names another layer, or a key whose `current` file is unreadable (so ancestors cannot be known),
    /// aborts the pass before anything is deleted. A record written by a newer devvm aborts it with that error alone.
    /// An unkept generation whose own record is unreadable is moved to quarantine, not deleted.
    pub fn gc(&self, reachable: &dyn Reachability) -> Result<GcReport> {
        let mut gc = self.gc_lock()?;
        gc.lock(
            Mode::Exclusive,
            "other devvm processes to finish with the layer store",
            &mut |m| self.progress.message(m),
        )?;
        let chains = reachable.chains()?;
        let instance_layers: HashSet<LayerRef> = chains.iter().flatten().copied().collect();
        let keys = self.key_dirs()?;

        let mut keep = instance_layers.clone();
        let mut currents: HashSet<LayerRef> = HashSet::new();
        for key in &keys {
            match self.read_current_file(key)? {
                CurrentFile::Absent => {}
                CurrentFile::Generation(generation) => {
                    let current = LayerRef { key: *key, generation };
                    keep.insert(current);
                    currents.insert(current);
                }
                CurrentFile::Unreadable(path) => {
                    return Err(Error::new(
                        ErrorKind::Store,
                        format!(
                            "cannot collect: {} is unreadable, so the generation that layer {key} keeps and that \
                             generation's ancestors are unknown; nothing was deleted; {}",
                            path.display(),
                            current_recovery(key, &path)
                        ),
                    ));
                }
            }
        }
        let mut pending: Vec<LayerRef> = keep.iter().copied().collect();
        // Popped in ascending order, so the same store always aborts on the same layer.
        pending.sort_by_key(|layer| Reverse(*layer));
        // The kept layers whose records name a layer as their parent, as the walk reads them.
        let mut children: HashMap<LayerRef, Vec<LayerRef>> = HashMap::new();
        // Why the walk cannot be trusted. The first stop is the one reported; the walk still runs to its end so the
        // abort names every kept child it read.
        let mut stop: Option<Stop> = None;
        while let Some(layer) = pending.pop() {
            let dir = self.generation_dir(&layer);
            let failure = match present(&dir) {
                // A kept layer that is gone has no ancestors to protect.
                Ok(false) => continue,
                Ok(true) => match meta::read(&dir) {
                    Ok(MetaRead::Valid(meta)) if meta.is_of(&layer) => {
                        if let Some(parent) = meta.parent {
                            children.entry(parent).or_default().push(layer);
                            if keep.insert(parent) {
                                pending.push(parent);
                            }
                        }
                        continue;
                    }
                    // A record of another layer says nothing about this one's ancestors.
                    Ok(MetaRead::Valid(_)) => Stop::Record(layer, Reason::OtherLayer),
                    Ok(MetaRead::Absent) => Stop::Record(layer, Reason::Missing),
                    Ok(MetaRead::Corrupt(why)) => Stop::Record(layer, Reason::Unreadable(why)),
                    // A newer schema passes through as it is.
                    Ok(MetaRead::Newer(e)) => Stop::Error(e),
                    Err(e) => Stop::Record(layer, Reason::ReadFailed(e)),
                },
                Err(e) => Stop::Error(e),
            };
            stop.get_or_insert(failure);
        }
        match stop {
            None => {}
            Some(Stop::Error(e)) => return Err(e),
            Some(Stop::Record(layer, reason)) => {
                let holders = holders(&layer, &instance_layers, &currents, &children);
                return Err(record_abort(&layer, &self.generation_dir(&layer), reason, &holders));
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
            match meta::read(&entry.path())? {
                MetaRead::Corrupt(why) => {
                    self.progress.message(&format!(
                        "layer record {} is unreadable ({why}); moving the layer to quarantine instead of deleting it",
                        entry.path().display()
                    ));
                    self.quarantine_record(&layer, true)?;
                    continue;
                }
                MetaRead::Newer(e) => return Err(e),
                MetaRead::Absent | MetaRead::Valid(_) => {}
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

/// What keeps a generation from being collected.
enum Holder {
    /// It is its key's `current`.
    Current(LayerKey),
    /// It is in the chain of an instance record.
    Instance,
    /// It is the recorded parent of this kept layer.
    Child(LayerRef),
}

/// Why `layer` is kept: its key's `current`, an instance chain, and the kept layers whose records name it as parent,
/// in the order of the layers. Only runs on the way to an abort.
fn holders(
    layer: &LayerRef,
    instance_layers: &HashSet<LayerRef>,
    currents: &HashSet<LayerRef>,
    children: &HashMap<LayerRef, Vec<LayerRef>>,
) -> Vec<Holder> {
    let mut holders = Vec::new();
    if currents.contains(layer) {
        holders.push(Holder::Current(layer.key));
    }
    if instance_layers.contains(layer) {
        holders.push(Holder::Instance);
    }
    let mut kept_children = children.get(layer).cloned().unwrap_or_default();
    kept_children.sort();
    holders.extend(kept_children.into_iter().map(Holder::Child));
    holders
}

impl std::fmt::Display for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Holder::Current(key) => write!(
                f,
                "the current generation of layer {key} (run `devvm build` for the recipe that produces it)"
            ),
            Holder::Instance => write!(f, "part of an instance's chain (remove that instance)"),
            Holder::Child(child) => write!(
                f,
                "the parent of layer {} generation {}, which is itself kept",
                child.key, child.generation
            ),
        }
    }
}

/// Where the walk could not go on.
enum Stop {
    /// The record of a kept layer says nothing usable about its ancestors.
    Record(LayerRef, Reason),
    /// An error to report as it is: a failed look at a kept layer's directory, or a record from a newer devvm.
    Error(Error),
}

/// What the walk found at a kept layer instead of a record that says what its ancestors are.
enum Reason {
    Missing,
    /// The record is there but unusable; the text says why.
    Unreadable(String),
    /// The record is valid but belongs to another layer.
    OtherLayer,
    /// Reading the record failed.
    ReadFailed(Error),
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reason::Missing => f.write_str("is missing"),
            Reason::Unreadable(why) => write!(f, "is unreadable ({why})"),
            Reason::OtherLayer => f.write_str("names another layer"),
            Reason::ReadFailed(cause) => write!(f, "cannot be read ({})", cause.message()),
        }
    }
}

/// The abort over the record of kept `layer` in `dir`. To make the store collectable again, every hold on the layer
/// must go, or the directory must move aside.
fn record_abort(layer: &LayerRef, dir: &Path, reason: Reason, holders: &[Holder]) -> Error {
    let holders: Vec<String> = holders.iter().map(Holder::to_string).collect();
    let message = format!(
        "cannot collect: the record of kept layer {} generation {} ({}) {reason}, so its ancestors are unknown; \
         nothing was deleted; it is kept as {}; to recover, move {} aside or release every hold on it",
        layer.key,
        layer.generation,
        dir.display(),
        holders.join(" and "),
        dir.display()
    );
    match reason {
        Reason::ReadFailed(cause) => Error::new(cause.kind(), message).with_source(cause),
        _ => Error::new(ErrorKind::Store, message),
    }
}

/// How to make a store collectable again after an abort over `key`'s unparseable `current` file at `path`.
fn current_recovery(key: &LayerKey, path: &std::path::Path) -> String {
    format!(
        "run `devvm build` for the recipe that produces layer {key} to replace it, or move {} aside",
        path.display()
    )
}

/// Whether the generation directory `dir` of a kept layer is there. A failed look is an error, never "gone": a kept
/// layer that is skipped leaves its parent unkept, and collection deletes the parent. Whatever is at the name counts as
/// there; the record read that follows rejects what is not a layer.
fn present(dir: &Path) -> Result<bool> {
    match symlink_metadata_if_present(dir) {
        Ok(found) => Ok(found.is_some()),
        Err(e) => {
            let look = look_failed(dir, e);
            Err(Error::new(ErrorKind::Io, format!("cannot collect: {look}; nothing was deleted")).with_source(look))
        }
    }
}

#[cfg(all(test, unix))]
#[path = "gc_unix_tests.rs"]
mod gc_unix_tests;
