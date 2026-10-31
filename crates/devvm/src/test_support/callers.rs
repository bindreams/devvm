//! The places a library caller may be blocked in when it calls the blocking SSH API.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use tokio::runtime::Builder;

/// Back-to-back calls from one task, more than tokio's cooperative budget of 128 units.
pub const MORE_THAN_THE_BUDGET: usize = 300;

/// Calls `call` from every kind of context a caller may be in, and fails naming each context where it panicked:
/// a plain thread, tokio's current-thread and multi-thread runtimes (inside a task, and inside `spawn_blocking`),
/// `futures`' `block_on` and `LocalPool`, and one tokio task making [`MORE_THAN_THE_BUDGET`] calls in a row.
pub fn in_every_caller_context(call: impl Fn() + Send + Sync + 'static) {
    let call = Arc::new(call);
    let mut failed = Vec::new();
    let mut check = |name: &'static str, body: &dyn Fn()| {
        if catch_unwind(AssertUnwindSafe(body)).is_err() {
            failed.push(name);
        }
    };

    check("a plain thread", &|| call());
    check("a tokio current-thread runtime", &|| {
        let call = call.clone();
        Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async move { call() });
    });
    check("a task of a tokio multi-thread runtime", &|| {
        let call = call.clone();
        let outer = Builder::new_multi_thread().worker_threads(2).build().unwrap();
        outer.block_on(async move { tokio::spawn(async move { call() }).await.unwrap() });
    });
    check("tokio's spawn_blocking", &|| {
        let call = call.clone();
        let outer = Builder::new_multi_thread().worker_threads(2).build().unwrap();
        outer.block_on(async move { tokio::task::spawn_blocking(move || call()).await.unwrap() });
    });
    check("futures::executor::block_on", &|| {
        let call = call.clone();
        futures::executor::block_on(async move { call() });
    });
    check("a futures LocalPool", &|| {
        let call = call.clone();
        futures::executor::LocalPool::new().run_until(async move { call() });
    });
    check("one tokio task making many calls in a row", &|| {
        let call = call.clone();
        let outer = Builder::new_multi_thread().worker_threads(2).build().unwrap();
        outer.block_on(async move {
            tokio::spawn(async move {
                for _ in 0..MORE_THAN_THE_BUDGET {
                    call();
                }
            })
            .await
            .unwrap()
        });
    });

    assert!(failed.is_empty(), "the call panicked in: {failed:?}");
}
