use std::cell::Cell;
use std::error::Error as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Barrier, mpsc};

use tokio::runtime::{Builder, Runtime};

use crate::lazy::Lazy;
use crate::{Error, ErrorKind};

#[skuld::test]
fn a_failed_build_is_not_kept_and_a_built_value_is_not_built_again() {
    let lazy = Lazy::<u32>::new();
    let builds = Cell::new(0);
    assert!(lazy.get(|| Err(Error::new(ErrorKind::Io, "no"))).is_err());
    assert_eq!(lazy.built(), None);

    let second = lazy.get(|| {
        builds.set(builds.get() + 1);
        Ok(7)
    });
    assert_eq!(second.unwrap(), &7);
    assert_eq!(lazy.built(), Some(&7));

    let third = lazy.get(|| {
        builds.set(builds.get() + 1);
        Ok(8)
    });
    assert_eq!(third.unwrap(), &7);
    assert_eq!(builds.get(), 1);
}

#[skuld::test]
fn the_error_of_a_failed_build_is_the_builds_own() {
    let lazy = Lazy::<u32>::new();
    let error = lazy
        .get(|| Err(Error::new(ErrorKind::Download, "the build").with_source(std::io::Error::other("the cause"))))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Download);
    assert_eq!(error.message(), "the build");
    assert_eq!(error.source().unwrap().to_string(), "the cause");
}

#[skuld::test]
fn a_panicking_build_leaves_the_next_get_able_to_build() {
    let lazy = Lazy::<u32>::new();
    assert!(catch_unwind(AssertUnwindSafe(|| lazy.get(|| panic!("the build panicked")))).is_err());
    assert_eq!(lazy.get(|| Ok(3)).unwrap(), &3);
}

#[skuld::test]
fn built_does_not_wait_for_a_build_in_progress() {
    let lazy = Lazy::<u32>::new();
    let (started, wait_for_start) = mpsc::channel();
    let (release, wait_for_release) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        let lazy = &lazy;
        let builder = scope.spawn(move || {
            lazy.get(|| {
                started.send(()).unwrap();
                wait_for_release.recv().unwrap();
                Ok(1)
            })
            .copied()
        });
        wait_for_start.recv().unwrap();
        assert_eq!(lazy.built(), None);
        release.send(()).unwrap();
        assert_eq!(builder.join().unwrap().unwrap(), 1);
    });
    assert_eq!(lazy.built(), Some(&1));
}

#[skuld::test]
fn racing_first_gets_from_async_contexts_build_once_and_do_not_panic() {
    const CALLERS: usize = 8;
    let lazy = Lazy::<Runtime>::new();
    let builds = AtomicUsize::new(0);
    let start = Barrier::new(CALLERS);
    let addresses: Vec<usize> = std::thread::scope(|scope| {
        let callers: Vec<_> = (0..CALLERS)
            .map(|_| {
                scope.spawn(|| {
                    // Each caller is inside its own runtime, where dropping a runtime panics: a value built to be
                    // thrown away would panic here.
                    Builder::new_current_thread().build().unwrap().block_on(async {
                        start.wait();
                        let built = lazy.get(|| {
                            builds.fetch_add(1, Ordering::SeqCst);
                            Ok(Builder::new_multi_thread().build().unwrap())
                        });
                        std::ptr::from_ref(built.unwrap()) as usize
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
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}
