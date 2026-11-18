//! The one in-flight mark of every sink: how an ending waits for the writes under way without ever waiting for a lock.
//!
//! A sink takes its own lock, then [`Gate::enter`]s, and holds the mark until it is done. The ender [`Gate::stop`]s each
//! sink and waits until none has a mark; it then writes without taking any sink's lock. A thread that panics inside a
//! sink still holds the lock and the mark when the panic hook runs on it, so every hook first calls
//! [`release_in_flight`]: the ender never waits for a thread that is waiting for the ender.

use std::cell::RefCell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

#[cfg(test)]
#[path = "gate_tests.rs"]
mod gate_tests;

/// A sink's stop flag and the writes of it that are in flight.
#[derive(Default)]
pub struct Gate {
    stopped: AtomicBool,
    flight: Arc<Flight>,
}

#[derive(Default)]
struct Flight {
    count: Mutex<usize>,
    idle: Condvar,
}

/// A write in flight. It ends when this is dropped, or when its thread calls [`release_in_flight`].
#[must_use]
pub struct Flying {
    mark: u64,
    // Marks belong to the thread that made them.
    _thread: PhantomData<*const ()>,
}

static NEXT_MARK: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The marks of this thread that have not ended.
    static MARKS: RefCell<Vec<(u64, Arc<Flight>)>> = const { RefCell::new(Vec::new()) };
}

impl Flight {
    fn leave(&self) {
        let mut count = self.count.lock().unwrap_or_else(PoisonError::into_inner);
        *count -= 1;
        if *count == 0 {
            self.idle.notify_all();
        }
    }
}

impl Gate {
    /// Marks a write in flight; `None`, with nothing marked, once the gate is stopped. The mark comes before the flag
    /// is read: whoever stops the gate waits for this write, or this write sees the stop.
    pub fn enter(&self) -> Option<Flying> {
        *self.flight.count.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        let mark = NEXT_MARK.fetch_add(1, Ordering::SeqCst);
        MARKS.with(|marks| marks.borrow_mut().push((mark, self.flight.clone())));
        let flying = Flying {
            mark,
            _thread: PhantomData,
        };
        (!self.is_stopped()).then_some(flying)
    }

    /// Makes every later [`Gate::enter`] return `None`.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// The writes in flight now.
    pub fn in_flight(&self) -> usize {
        *self.flight.count.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits until no write is in flight.
    pub fn wait_idle(&self) {
        let mut count = self.flight.count.lock().unwrap_or_else(PoisonError::into_inner);
        while *count > 0 {
            count = self.flight.idle.wait(count).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Stops every gate, then waits until none has a write in flight.
    pub fn stop_and_wait(gates: &[&Gate]) {
        for gate in gates {
            gate.stop();
        }
        for gate in gates {
            gate.wait_idle();
        }
    }
}

impl Drop for Flying {
    fn drop(&mut self) {
        let flight = MARKS
            .try_with(|marks| {
                let mut marks = marks.borrow_mut();
                let at = marks.iter().position(|(mark, _)| *mark == self.mark)?;
                Some(marks.remove(at).1)
            })
            .ok()
            .flatten();
        if let Some(flight) = flight {
            flight.leave();
        }
    }
}

/// Ends every write this thread has in flight, on any gate; they are never resumed. Called first by every panic hook,
/// on the panicking thread, which may be inside a write. A thread with none has nothing to release.
pub fn release_in_flight() {
    let marks = MARKS.try_with(RefCell::take).unwrap_or_default();
    for (_, flight) in marks {
        flight.leave();
    }
}
