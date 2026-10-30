use super::{End, ExitWatch};
use crate::backend::qemu::uds::UnixStream;
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
    let watch = ExitWatch::new();

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
    let watch = ExitWatch::new();
    let registration = watch.register(client).unwrap();

    watch.report_exit();

    assert!(watch.lock().connection.is_none());
    assert!(registration.exited());
    drop(registration);
}

#[skuld::test]
fn nothing_registers_after_the_exit() {
    let (_sock, client, _peer) = connected();
    let watch = ExitWatch::new();
    watch.report_exit();

    assert!(matches!(watch.register(client), Err(End::Exited)));
}

#[skuld::test]
fn the_first_end_stands() {
    let watch = ExitWatch::new();
    watch.report_exit();
    watch.report_unwatchable("late".to_owned());

    assert!(matches!(watch.ended(), Some(End::Exited)));
}

#[skuld::test]
fn pause_returns_at_once_when_the_watch_has_ended() {
    let watch = ExitWatch::new();
    watch.report_unwatchable("gone".to_owned());

    assert!(matches!(watch.pause(), Some(End::Unwatchable(why)) if why == "gone"));
}
