//! Whether a QEMU process has exited, learned from the process and not from its control socket.
//!
//! Windows can lose the end of an AF_UNIX stream: a reader blocked on a socket whose peer died may never wake. One
//! thread per machine waits for the process to exit and, when it does, interrupts the connection devvm holds to it, so
//! a blocked read returns at once and what QEMU wrote before it died is still read. The wait is the OS's, not a timer.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::uds::UnixStream;
use crate::Result;
use crate::host::Host;

#[cfg(test)]
#[path = "exit_watch_tests.rs"]
mod exit_watch_tests;

/// How long a wait for the control socket to appear lasts before it tries again. An exit ends the wait at once.
const RETRY: Duration = Duration::from_millis(10);

/// Why a watch ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// The process exited.
    Exited,
    /// The OS would not say when the process exits (the text says why); it may be alive. This fails closed: a session
    /// on such a watch could hang on a dead QEMU with nothing to wake it, so `Qmp::connect` refuses it.
    Unwatchable(String),
}

/// The exit of one process. Clones share it.
#[derive(Clone)]
pub(crate) struct ExitWatch {
    shared: Arc<Shared>,
}

struct Shared {
    host: &'static dyn Host,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    end: Option<End>,
    /// The connection to wake on exit: a clone of the live session's socket, owned by the watch until the session
    /// unregisters or the exit interrupts it. Both take it out of this slot under the mutex, so the interrupt never runs
    /// on a closed handle and a closing session never races it.
    connection: Option<UnixStream>,
    /// Why waking the connection failed, if it did. The reader may then have been woken another way, or not at all; the
    /// session says so in the error it returns.
    wake_failure: Option<String>,
}

impl ExitWatch {
    /// A watch that reports an exit only when told to (`report_exit`).
    pub(crate) fn new(host: &'static dyn Host) -> ExitWatch {
        ExitWatch {
            shared: Arc::new(Shared {
                host,
                state: Mutex::default(),
                changed: Condvar::new(),
            }),
        }
    }

    /// A watch of the process `wait` blocks on. The thread it starts ends when `wait` returns, which is when the process
    /// exits; it holds nothing of the machine.
    pub(crate) fn watching(host: &'static dyn Host, wait: impl FnOnce() -> Result<()> + Send + 'static) -> ExitWatch {
        let watch = ExitWatch::new(host);
        let thread_watch = watch.clone();
        let started = std::thread::Builder::new()
            .name("qemu-exit-watch".to_owned())
            .spawn(move || match wait() {
                Ok(()) => thread_watch.report_exit(),
                Err(e) => thread_watch.report_unwatchable(e.to_string()),
            });
        if let Err(e) = started {
            watch.report_unwatchable(format!("cannot start the thread that waits for the process: {e}"));
        }
        watch
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the exit and wakes everything that waits on it: the registered connection is interrupted, so a read
    /// blocked on it returns, and a connect wait ends. A failed interrupt is kept for the session's error.
    pub(crate) fn report_exit(&self) {
        let mut state = self.lock();
        state.end.get_or_insert(End::Exited);
        if let Some(connection) = state.connection.take()
            && let Err(e) = self.shared.host.interrupt_connection(&connection)
        {
            state.wake_failure = Some(e.to_string());
        }
        self.shared.changed.notify_all();
    }

    fn report_unwatchable(&self, why: String) {
        self.lock().end.get_or_insert(End::Unwatchable(why));
        self.shared.changed.notify_all();
    }

    /// How the watch ended, if it has.
    pub(crate) fn ended(&self) -> Option<End> {
        self.lock().end.clone()
    }

    /// Blocks until the watch ends and says how. There is no limit: the end is the process exiting, or the OS refusing to say.
    pub(crate) fn wait_ended(&self) -> End {
        let mut state = self.lock();
        loop {
            if let Some(end) = state.end.clone() {
                return end;
            }
            state = self.shared.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Waits for the watch to end or for `RETRY` to pass, whichever is first; the caller re-checks its own condition.
    pub(crate) fn pause(&self) -> Option<End> {
        self.pause_for(RETRY)
    }

    fn pause_for(&self, limit: Duration) -> Option<End> {
        let state = self.lock();
        if state.end.is_some() {
            return state.end.clone();
        }
        let (state, _) = self
            .shared
            .changed
            .wait_timeout(state, limit)
            .unwrap_or_else(PoisonError::into_inner);
        state.end.clone()
    }

    /// Hands `connection` (a clone of the session's socket) to the watch, to be interrupted if the process exits. Fails
    /// with how the watch ended when it has. One session at a time: the control socket's lock file guarantees it.
    pub(crate) fn register(&self, connection: UnixStream) -> std::result::Result<Registration, End> {
        let mut state = self.lock();
        if let Some(end) = state.end.clone() {
            return Err(end);
        }
        debug_assert!(state.connection.is_none(), "two sessions on one control socket");
        state.connection = Some(connection);
        Ok(Registration { watch: self.clone() })
    }
}

/// A session's place in the watch. Dropping it takes the connection back out, so the watch never holds a socket open
/// past its session.
pub(crate) struct Registration {
    watch: ExitWatch,
}

impl Registration {
    /// Whether the process is known to have exited.
    pub(crate) fn exited(&self) -> bool {
        self.watch.ended() == Some(End::Exited)
    }

    /// Why waking this session's connection failed, if the exit tried and failed.
    pub(crate) fn wake_failure(&self) -> Option<String> {
        self.watch.lock().wake_failure.clone()
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.watch.lock().connection = None;
    }
}
