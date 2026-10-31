use super::{End, ExitWatch};
use crate::backend::qemu::uds::UnixStream;
use crate::host;
use crate::test_support::qemu::Sock;

/// A connected stream; the peer is kept by the caller.
fn connected() -> (Sock, UnixStream, UnixStream) {
    let sock = Sock::new();
    let listener = sock.listen();
    let client = UnixStream::connect(sock.endpoint.path()).unwrap();
    let (peer, _) = listener.accept().unwrap();
    (sock, client, peer)
}

#[skuld::test]
fn a_registered_connection_is_held_until_the_registration_drops() {
    let (_sock, client, _peer) = connected();
    let watch = ExitWatch::new(host::current());

    let registration = watch.register(client).unwrap();
    assert!(watch.lock().connection.is_some());
    drop(registration);

    assert!(
        watch.lock().connection.is_none(),
        "the watch must not keep the socket open"
    );
}

#[skuld::test]
fn an_exit_releases_the_connection_it_interrupted() {
    let (_sock, client, _peer) = connected();
    let watch = ExitWatch::new(host::current());
    let registration = watch.register(client).unwrap();

    watch.report_exit();

    assert!(watch.lock().connection.is_none());
    assert!(registration.exited());
    drop(registration);
}

#[skuld::test]
fn nothing_registers_after_the_exit() {
    let (_sock, client, _peer) = connected();
    let watch = ExitWatch::new(host::current());
    watch.report_exit();

    assert!(matches!(watch.register(client), Err(End::Exited)));
}

#[skuld::test]
fn the_first_end_stands() {
    let watch = ExitWatch::new(host::current());
    watch.report_exit();
    watch.report_unwatchable("late".to_owned());

    assert!(matches!(watch.ended(), Some(End::Exited)));
}

#[skuld::test]
fn pause_returns_at_once_when_the_watch_has_ended() {
    let watch = ExitWatch::new(host::current());
    watch.report_unwatchable("gone".to_owned());

    assert!(matches!(watch.pause(), Some(End::Unwatchable(why)) if why == "gone"));
}

#[skuld::test]
fn a_watched_process_that_exits_ends_the_watch_as_exited() {
    let watch = ExitWatch::watching(host::current(), || Ok(()));

    assert_eq!(watch.wait_ended(), End::Exited);
    assert_eq!(watch.ended(), Some(End::Exited));
}

#[skuld::test]
fn a_wait_the_os_refuses_ends_the_watch_as_unwatchable_naming_why() {
    let watch = ExitWatch::watching(host::current(), || {
        Err(crate::Error::new(crate::ErrorKind::Io, "the wait was refused"))
    });

    let End::Unwatchable(why) = watch.wait_ended() else {
        panic!("a refused wait is not an exit");
    };
    assert!(why.contains("the wait was refused"), "{why}");
}

#[skuld::test]
fn a_registration_on_a_watch_that_cannot_tell_does_not_say_the_process_exited() {
    let (_sock, client, _peer) = connected();
    let watch = ExitWatch::new(host::current());
    let registration = watch.register(client).unwrap();

    watch.report_unwatchable("gone".to_owned());

    assert!(!registration.exited());
}

#[skuld::test]
fn an_exit_reported_during_a_pause_ends_it() {
    let watch = ExitWatch::new(host::current());

    // An hour: only the report can end this pause in time, and a pause it does not wake never returns.
    let ended = std::thread::scope(|scope| {
        let pause = scope.spawn(|| watch.pause_for(std::time::Duration::from_secs(3600)));
        watch.report_exit();
        pause.join().unwrap()
    });

    assert_eq!(ended, Some(End::Exited));
}

#[skuld::test]
fn an_exit_reported_while_waiting_for_the_end_ends_the_wait() {
    let watch = ExitWatch::new(host::current());

    let end = std::thread::scope(|scope| {
        let wait = scope.spawn(|| watch.wait_ended());
        watch.report_exit();
        wait.join().unwrap()
    });

    assert_eq!(end, End::Exited);
}
