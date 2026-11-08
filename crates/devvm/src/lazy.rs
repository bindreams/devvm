//! A value built on first use.

use std::sync::{Mutex, OnceLock, PoisonError};

#[cfg(test)]
#[path = "lazy_tests.rs"]
mod lazy_tests;

/// A value built by the first `get` that succeeds. The build is serialized, so a value is never built to be thrown
/// away: a build may be expensive or have side effects, and dropping a tokio runtime panics in an asynchronous
/// context, where a library caller may be.
pub(crate) struct Lazy<T> {
    value: OnceLock<T>,
    building: Mutex<()>,
}

impl<T> Lazy<T> {
    pub(crate) const fn new() -> Self {
        Self {
            value: OnceLock::new(),
            building: Mutex::new(()),
        }
    }

    /// The value, built with `build` if there is none yet. A failed build is not kept: the next `get` builds again,
    /// until one succeeds. Concurrent callers build one at a time, and a caller whose turn comes after a success
    /// does not build.
    pub(crate) fn get<E>(&self, build: impl FnOnce() -> Result<T, E>) -> Result<&T, E> {
        if let Some(value) = self.value.get() {
            return Ok(value);
        }
        // Nothing under the lock can leave the guarded data (`()`) wrong, so a poisoned lock is as good as a clean one.
        let _building = self.building.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(value) = self.value.get() {
            return Ok(value);
        }
        let built = build()?;
        Ok(self.value.get_or_init(|| built))
    }

    /// The value if it is already built; never builds, and does not wait for a build in progress.
    pub(crate) fn built(&self) -> Option<&T> {
        self.value.get()
    }
}

impl<T> Default for Lazy<T> {
    fn default() -> Self {
        Self::new()
    }
}
