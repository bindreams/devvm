//! The one tokio runtime behind the blocking SSH API.
//!
//! Work is handed to it with `spawn` and awaited with a plain blocking wait on a channel, never `Runtime::block_on`
//! or `Handle::block_on`: those panic inside another runtime, and a library caller may be in its own async test.

use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

use tokio::runtime::{Builder, Runtime};

use crate::exit::Progress;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod runtime_tests;

/// A runtime started on first use. The start is serialized, so a runtime is never built to be thrown away: dropping
/// one panics in an asynchronous context, where a library caller may be.
pub(crate) struct Lazy {
    runtime: OnceLock<Runtime>,
    starting: Mutex<()>,
}

impl Lazy {
    pub(crate) const fn new() -> Self {
        Self {
            runtime: OnceLock::new(),
            starting: Mutex::new(()),
        }
    }

    pub(crate) fn get(&self) -> Result<&Runtime> {
        if let Some(runtime) = self.runtime.get() {
            return Ok(runtime);
        }
        // Nothing under the lock can leave the guarded data (`()`) wrong, so a poisoned lock is as good as a clean one.
        let _starting = self.starting.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(runtime) = self.runtime.get() {
            return Ok(runtime);
        }
        let started = Builder::new_multi_thread()
            .thread_name("devvm-ssh")
            .enable_all()
            .build()
            .map_err(|e| Error::io(ErrorKind::Io, "cannot start the threads of the SSH runtime", e))?;
        Ok(self.runtime.get_or_init(|| started))
    }
}

fn runtime() -> Result<&'static Runtime> {
    static RUNTIME: Lazy = Lazy::new();
    RUNTIME.get()
}

/// Starts `task` on the runtime.
pub(crate) fn spawn<F>(task: F) -> Result<tokio::task::JoinHandle<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    Ok(runtime()?.spawn(task))
}

/// Wakes the thread that is parked in [`wait`].
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Waits for `future` on the calling thread. The future may be a tokio channel receiver or a join handle: those do
/// not need to be polled by a tokio runtime, so this is safe inside one. It enters no executor, so it is as safe
/// inside `futures`' executors, which panic when entered twice. It runs outside the cooperative budget of an
/// enclosing tokio task: a wait that never returns to the scheduler would never get the budget back and would hang
/// once it ran out.
pub(crate) fn wait<F: Future>(future: F) -> F::Output {
    let mut future = pin!(tokio::task::unconstrained(future));
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        // A wake that came before this park left a token, so the park returns at once; a wake with nothing new only
        // costs one more poll.
        thread::park();
    }
}

fn panicked(what: &str) -> Error {
    Error::new(ErrorKind::Internal, format!("the SSH task for {what} panicked"))
}

/// Runs `task` to completion on the runtime and returns its output.
pub(crate) fn run<T>(what: &str, task: impl Future<Output = T> + Send + 'static) -> Result<T>
where
    T: Send + 'static,
{
    wait(spawn(task)?).map_err(|_| panicked(what))
}

/// [`run`], where the task also sends progress messages, which are handed to `progress` as they arrive.
pub(crate) fn run_reporting<T, F, Fut>(what: &str, progress: &dyn Progress, task: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(tokio::sync::mpsc::UnboundedSender<String>) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
{
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let handle = spawn(task(sender))?;
    // The sender lives inside the task, so the channel closes exactly when the task is over or gone.
    while let Some(message) = wait(receiver.recv()) {
        progress.message(&message);
    }
    wait(handle).map_err(|_| panicked(what))
}
