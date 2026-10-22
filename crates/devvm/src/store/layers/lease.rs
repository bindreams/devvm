//! Leases: shared locks that keep generations alive for as long as someone uses them.

use super::{LayerDir, LayerMeta, LayerRef};
use crate::store::lock::FileLock;

/// A shared lock on one generation, plus its record. While it lives, garbage collection skips the generation.
#[derive(Debug)]
pub struct LayerLease {
    meta: LayerMeta,
    dir: LayerDir,
    _lock: FileLock,
}

impl LayerLease {
    pub(super) fn new(meta: LayerMeta, dir: LayerDir, lock: FileLock) -> LayerLease {
        LayerLease { meta, dir, _lock: lock }
    }

    pub fn meta(&self) -> &LayerMeta {
        &self.meta
    }

    pub fn dir(&self) -> &LayerDir {
        &self.dir
    }

    pub fn layer_ref(&self) -> LayerRef {
        LayerRef {
            key: self.meta.key,
            generation: self.meta.generation,
        }
    }
}

/// Leases on every generation of a chain, root first. An image handle holds one for its whole life.
#[derive(Debug)]
pub struct ChainLease {
    layers: Vec<LayerLease>,
}

impl ChainLease {
    pub(super) fn new(layers: Vec<LayerLease>) -> ChainLease {
        debug_assert!(!layers.is_empty());
        ChainLease { layers }
    }

    /// Root first.
    pub fn layers(&self) -> &[LayerLease] {
        &self.layers
    }

    pub fn tip(&self) -> &LayerLease {
        self.layers.last().expect("a chain has at least one layer")
    }

    /// Root first.
    pub fn refs(&self) -> Vec<LayerRef> {
        self.layers.iter().map(LayerLease::layer_ref).collect()
    }
}
