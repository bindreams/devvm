//! The session against a QEMU that exits while its end of the connection stays open. Windows only: on a host whose
//! streams end with the peer's process, no test can keep the peer's end open past its exit. Each test is thread-only (the
//! "process" is an event the test sets), so none of them spawn anything.

use std::sync::mpsc;

use serde_json::json;

use super::qmp_tests::{Server, Session, serve, sock};
use super::{Qmp, VmEnd};
use crate::ErrorKind;
use crate::host::ProcessWatch;
use crate::test_support::fixtures::MemoryProgress;
use crate::test_support::manual_exit::ManualExit;
use crate::test_support::qemu::Sock;
use std::sync::Arc;

/// A connection to a server that wrote `lines` on its own right after the handshake and then holds the connection open
/// (a stream whose end Windows may lose). Returns once the server has written them.
fn connected_to_a_server_that_wrote(
    sock: &Sock,
    process: &Arc<dyn ProcessWatch>,
    lines: impl Fn(&mut Session) + Send + 'static,
) -> (Server, Qmp) {
    let (written, written_rx) = mpsc::channel::<()>();
    let (server, _) = serve(sock.listen(), 1, move |session, _| {
        lines(session);
        written.send(()).unwrap();
        while session.command().is_some() {}
    });
    let qmp = Qmp::connect(&sock.endpoint, process, &MemoryProgress::default()).unwrap();
    written_rx.recv().unwrap();
    (server, qmp)
}

#[skuld::test]
fn a_reply_that_arrived_before_the_exit_is_read_after_it() {
    let sock = sock();
    let process = ManualExit::new();
    let (server, mut qmp) =
        connected_to_a_server_that_wrote(&sock, &process.watch(), |session| session.reply(json!({})));
    process.exit();

    // `quit`'s reply is the one QEMU writes just before it exits.
    qmp.quit().unwrap();

    drop(qmp);
    server.join().unwrap();
}

#[skuld::test]
fn a_no_reboot_shutdown_that_arrived_before_the_exit_is_still_read() {
    let sock = sock();
    let process = ManualExit::new();
    let (server, mut qmp) = connected_to_a_server_that_wrote(&sock, &process.watch(), |session| {
        session.reply(json!({}));
        session.event(
            "SHUTDOWN",
            Some(json!({ "guest": false, "reason": "host-qmp-system-reset" })),
        );
    });
    process.exit();

    let error = qmp.system_reset().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("no-reboot"), "{error}");
    drop(qmp);
    server.join().unwrap();
}

#[skuld::test]
fn the_read_after_the_last_buffered_event_of_an_exited_process_ends_instead_of_blocking() {
    let sock = sock();
    let process = ManualExit::new();
    let (server, mut qmp) =
        connected_to_a_server_that_wrote(&sock, &process.watch(), |session| session.event("POWERDOWN", None));
    process.exit();

    // The server still holds the connection open: only the exit can end this wait.
    assert_eq!(qmp.wait_for_end().unwrap(), VmEnd::Exited);

    drop(qmp);
    server.join().unwrap();
}

#[skuld::test]
fn a_process_that_exits_while_a_read_is_blocked_ends_the_read() {
    let sock = sock();
    let process = ManualExit::new();
    let (asked, asked_rx) = mpsc::channel();
    let (finished, finished_rx) = mpsc::channel::<()>();
    // The server takes the command and never answers; it keeps the connection open until the client is done, so the
    // read can end only because the process exited.
    let (server, _) = serve(sock.listen(), 1, move |session, _| {
        let (name, _) = session.command().unwrap();
        asked.send(name).unwrap();
        let _ = finished_rx.recv();
    });
    let mut qmp = Qmp::connect(&sock.endpoint, &process.watch(), &MemoryProgress::default()).unwrap();

    let error = std::thread::scope(|scope| {
        let client = scope.spawn(move || qmp.query_status());
        assert_eq!(asked_rx.recv().unwrap(), "query-status");
        process.exit();
        client.join().unwrap().unwrap_err()
    });

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    finished.send(()).unwrap();
    server.join().unwrap();
}

#[skuld::test]
fn a_read_that_starts_after_the_process_exited_ends_at_once() {
    let sock = sock();
    let process = ManualExit::new();
    let (finished, finished_rx) = mpsc::channel::<()>();
    // The server keeps the connection open and says nothing, so a read can end only because the process exited.
    let (server, _) = serve(sock.listen(), 1, move |_, _| {
        let _ = finished_rx.recv();
    });
    let mut qmp = Qmp::connect(&sock.endpoint, &process.watch(), &MemoryProgress::default()).unwrap();

    process.exit();
    assert_eq!(qmp.wait_for_end().unwrap(), VmEnd::Exited);
    assert_eq!(qmp.wait_for_end().unwrap(), VmEnd::Exited, "a second read");
    let error = qmp.query_status().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    finished.send(()).unwrap();
    server.join().unwrap();
}

#[skuld::test]
fn a_command_sent_after_the_process_exited_is_a_lost_connection() {
    let sock = sock();
    let process = ManualExit::new();
    let (finished, finished_rx) = mpsc::channel::<()>();
    let (server, _) = serve(sock.listen(), 1, move |_, _| {
        let _ = finished_rx.recv();
    });
    let mut qmp = Qmp::connect(&sock.endpoint, &process.watch(), &MemoryProgress::default()).unwrap();

    process.exit();
    let error = qmp.stop().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    finished.send(()).unwrap();
    server.join().unwrap();
}
