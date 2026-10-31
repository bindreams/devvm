//! The one tokio runtime behind the blocking SSH API.
//!
//! Work is handed to it with `spawn` and awaited with a plain blocking wait on a channel, never `Runtime::block_on`
//! or `Handle::block_on`: those panic inside another runtime, and a library caller may be in its own async test.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::{Builder, Runtime};

use crate::exit::Progress;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod runtime_tests;

/// The runtime, started on first use. Two first uses at once may each start one; the loser is dropped unused.
fn runtime() -> Result<&'static Runtime> {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let started = Builder::new_multi_thread()
        .thread_name("devvm-ssh")
        .enable_all()
        .build()
        .map_err(|e| Error::io(ErrorKind::Io, "cannot start the threads of the SSH runtime", e))?;
    Ok(RUNTIME.get_or_init(|| started))
}

/// Starts `task` on the runtime.
pub(crate) fn spawn<F>(task: F) -> Result<tokio::task::JoinHandle<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    Ok(runtime()?.spawn(task))
}

/// Waits for `future` on the calling thread. The future may be a tokio channel receiver or a join handle: those do
/// not need to be polled by a tokio runtime, so this is safe inside one.
pub(crate) fn wait<F: Future>(future: F) -> F::Output {
    futures::executor::block_on(future)
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
