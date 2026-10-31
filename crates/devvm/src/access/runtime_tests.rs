use crate::ErrorKind;
use crate::access::runtime::{Lazy, run, run_reporting};
use crate::test_support::fixtures::MemoryProgress;

#[skuld::test]
fn run_returns_the_output_of_the_task() {
    assert_eq!(run("a sum", async { 2 + 2 }).unwrap(), 4);
}

#[skuld::test]
fn a_panicking_task_is_an_internal_error_naming_it() {
    let error = run::<()>("the boom", async { panic!("boom") }).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Internal);
    assert!(error.message().contains("the boom"), "{error}");
}

#[skuld::test]
fn run_works_inside_another_tokio_runtime() {
    let outer = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let value = outer.block_on(async {
        tokio::spawn(async { run("a sum", async { 7 }).unwrap() })
            .await
            .unwrap()
    });
    assert_eq!(value, 7);
}

#[skuld::test]
fn run_reporting_hands_over_every_message_before_the_result() {
    let progress = MemoryProgress::default();
    let out = run_reporting("a count", &progress, |messages| async move {
        for n in 0..3 {
            messages.send(format!("step {n}")).unwrap();
        }
        "done"
    })
    .unwrap();
    assert_eq!(out, "done");
    assert_eq!(progress.messages(), ["step 0", "step 1", "step 2"]);
}

/// Calls made back to back from one outer task: each one uses up a unit of tokio's cooperative budget.
const MORE_THAN_THE_BUDGET: usize = 300;

#[skuld::test]
fn many_run_calls_inside_one_outer_task_complete() {
    let outer = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let sum = outer.block_on(async {
        tokio::spawn(async {
            (0..MORE_THAN_THE_BUDGET)
                .map(|n| run("a number", async move { n }).unwrap())
                .sum::<usize>()
        })
        .await
        .unwrap()
    });
    assert_eq!(sum, (0..MORE_THAN_THE_BUDGET).sum::<usize>());
}

#[skuld::test]
fn many_progress_messages_inside_one_outer_task_arrive() {
    let outer = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let progress = std::sync::Arc::new(MemoryProgress::default());
    outer.block_on({
        let progress = progress.clone();
        async move {
            tokio::spawn(async move {
                run_reporting("a count", &*progress, |messages| async move {
                    for n in 0..MORE_THAN_THE_BUDGET {
                        messages.send(n.to_string()).unwrap();
                    }
                })
                .unwrap();
            })
            .await
            .unwrap()
        }
    });
    assert_eq!(progress.messages().len(), MORE_THAN_THE_BUDGET);
}

#[skuld::test]
fn racing_first_uses_from_async_contexts_do_not_panic() {
    const CALLERS: usize = 8;
    let lazy = Lazy::new();
    let start = std::sync::Barrier::new(CALLERS);
    let addresses: Vec<usize> = std::thread::scope(|scope| {
        let callers: Vec<_> = (0..CALLERS)
            .map(|_| {
                scope.spawn(|| {
                    // Each caller is inside its own runtime, where dropping a runtime panics.
                    tokio::runtime::Builder::new_current_thread()
                        .build()
                        .unwrap()
                        .block_on(async {
                            start.wait();
                            std::ptr::from_ref(lazy.get().unwrap()) as usize
                        })
                })
            })
            .collect();
        callers
            .into_iter()
            .map(|caller| caller.join().expect("a caller panicked"))
            .collect()
    });
    assert!(addresses.windows(2).all(|pair| pair[0] == pair[1]), "{addresses:?}");
}
