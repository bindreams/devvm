use std::sync::mpsc;

use crate::progress::gate::{Gate, release_in_flight};

#[skuld::test]
fn a_mark_is_in_flight_until_it_is_dropped() {
    let gate = Gate::default();
    let first = gate.enter().expect("not stopped");
    let second = gate.enter().expect("marks nest");
    assert_eq!(gate.in_flight(), 2);
    drop(first);
    assert_eq!(gate.in_flight(), 1);
    drop(second);
    assert_eq!(gate.in_flight(), 0);
}

#[skuld::test]
fn a_stopped_gate_marks_nothing() {
    let gate = Gate::default();
    gate.stop();
    assert!(gate.enter().is_none());
    assert_eq!(gate.in_flight(), 0);
}

#[skuld::test]
fn releasing_ends_every_mark_of_the_thread_on_every_gate_and_the_drops_after_it_change_nothing() {
    let (a, b) = (Gate::default(), Gate::default());
    let in_a = a.enter().unwrap();
    let in_b = b.enter().unwrap();
    let in_a_again = a.enter().unwrap();

    release_in_flight();
    assert_eq!((a.in_flight(), b.in_flight()), (0, 0));

    // A mark made after the release is a new one; the old guards must not end it.
    let later = a.enter().unwrap();
    drop((in_a, in_b, in_a_again));
    assert_eq!(a.in_flight(), 1);
    drop(later);
    assert_eq!(a.in_flight(), 0);
}

#[skuld::test]
fn releasing_on_a_thread_with_no_mark_does_nothing() {
    let gate = Gate::default();
    release_in_flight();
    let mark = gate.enter().unwrap();
    std::thread::scope(|scope| scope.spawn(release_in_flight).join().unwrap());
    assert_eq!(
        gate.in_flight(),
        1,
        "another thread's release does not end this thread's mark"
    );
    drop(mark);
    assert_eq!(gate.in_flight(), 0);
}

#[skuld::test]
fn waiting_ends_when_the_other_thread_drops_its_mark() {
    let gate = &Gate::default();
    let (entered, seen) = mpsc::channel();
    let (leave, left) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let mark = gate.enter().unwrap();
            entered.send(()).unwrap();
            left.recv().unwrap();
            drop(mark);
        });
        seen.recv().unwrap();
        gate.stop();
        assert!(gate.enter().is_none());
        assert_eq!(gate.in_flight(), 1);
        leave.send(()).unwrap();
        Gate::stop_and_wait(&[gate]);
        assert_eq!(gate.in_flight(), 0);
    });
}

/// The thread inside the write panics, and its hook releases the mark while the write's lock is still held: the
/// waiter is let go without that thread leaving the write.
#[skuld::test]
fn waiting_ends_when_the_other_thread_releases_its_mark_inside_its_write() {
    let gate = &Gate::default();
    let (entered, seen) = mpsc::channel();
    let (hooked, after_hook) = mpsc::channel();
    let (finish, finished) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let _mark = gate.enter().unwrap();
            entered.send(()).unwrap();
            while !gate.is_stopped() {
                std::thread::yield_now();
            }
            release_in_flight();
            hooked.send(()).unwrap();
            // Still inside the write.
            finished.recv().unwrap();
        });
        seen.recv().unwrap();
        gate.stop();
        gate.wait_idle();
        after_hook.recv().unwrap();
        finish.send(()).unwrap();
    });
    assert_eq!(gate.in_flight(), 0);
}
