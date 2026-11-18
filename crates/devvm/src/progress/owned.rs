//! A mutex whose holder is known, for state that a panic hook may need on the thread that holds it.

use std::ops::{Deref, DerefMut};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::{self, ThreadId};

pub(crate) struct Owned<T> {
    value: Mutex<T>,
    holder: Mutex<Option<ThreadId>>,
}

/// The value, locked; the lock is released and the holder forgotten when this is dropped.
pub(crate) struct Held<'a, T> {
    value: MutexGuard<'a, T>,
    holder: &'a Mutex<Option<ThreadId>>,
}

impl<T> Owned<T> {
    pub(crate) fn new(value: T) -> Owned<T> {
        Owned {
            value: Mutex::new(value),
            holder: Mutex::new(None),
        }
    }

    /// The value, locked, ignoring poison; `None` when this thread holds it already, where waiting would wait for
    /// itself.
    pub(crate) fn lock(&self) -> Option<Held<'_, T>> {
        let me = thread::current().id();
        if *self.holder.lock().unwrap_or_else(PoisonError::into_inner) == Some(me) {
            return None;
        }
        let value = self.value.lock().unwrap_or_else(PoisonError::into_inner);
        *self.holder.lock().unwrap_or_else(PoisonError::into_inner) = Some(me);
        Some(Held {
            value,
            holder: &self.holder,
        })
    }
}

impl<T> Deref for Held<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Held<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T> Drop for Held<'_, T> {
    fn drop(&mut self) {
        *self.holder.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}
