use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::{Qmp, QmpEndpoint, VmEnd};
use crate::ErrorKind;
use crate::backend::qemu::exit_watch::ExitWatch;
use crate::backend::qemu::ports::Forward;
use crate::backend::qemu::uds::{UnixListener, UnixStream};
use crate::host;
use crate::instance::record::InstanceId;
use crate::test_support::fixtures::{ChannelProgress, MemoryProgress, UNICODE_DIR};
use crate::test_support::qemu::Sock;

// A fake QMP server ===================================================================================================

/// One accepted connection, speaking QMP as QEMU does.
pub(crate) struct Session {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    /// Ended by `Server::join`: the test is over with the client, and a read that waits for the client's end of stream
    /// must return even if Windows lost that end of stream.
    release: ExitWatch,
}

impl Session {
    fn new(stream: UnixStream, release: ExitWatch) -> Session {
        Session {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
            release,
        }
    }

    fn send(&mut self, value: Value) {
        let mut line = value.to_string();
        line.push('\n');
        self.send_raw(line.as_bytes());
    }

    /// Writes `bytes` as they are: a line that is not JSON, or not text, or has no end.
    pub(crate) fn send_raw(&mut self, bytes: &[u8]) {
        // The client may be gone already; a script that cares reads first.
        let _ = self.writer.write_all(bytes).and_then(|()| self.writer.flush());
    }

    /// The next command as `(name, arguments)`; `None` when the client closed the connection or the test released the
    /// server (`Server::join`).
    pub(crate) fn command(&mut self) -> Option<(String, Value)> {
        let _registration = self.release.register(self.writer.try_clone().unwrap()).ok()?;
        let mut line = String::new();
        if self.reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let value: Value = serde_json::from_str(&line).unwrap();
        Some((
            value["execute"].as_str().unwrap().to_owned(),
            value.get("arguments").cloned().unwrap_or(Value::Null),
        ))
    }

    pub(crate) fn reply(&mut self, value: Value) {
        self.send(json!({ "return": value }));
    }

    fn fail(&mut self, desc: &str) {
        self.send(json!({ "error": { "class": "GenericError", "desc": desc } }));
    }

    fn event(&mut self, name: &str, data: Option<Value>) {
        let mut event = json!({ "event": name, "timestamp": { "seconds": 1, "microseconds": 2 } });
        if let Some(data) = data {
            event["data"] = data;
        }
        self.send(event);
    }

    /// Replies to the next command with `value` and returns its name.
    pub(crate) fn answer(&mut self, value: Value) -> String {
        let (name, _) = self.command().expect("a command was expected");
        self.reply(value);
        name
    }
}

/// QEMU's greeting.
fn greeting() -> Value {
    json!({
        "QMP": {
            "version": { "qemu": { "micro": 2, "minor": 1, "major": 11 }, "package": "v11.1.2" },
            "capabilities": ["oob"]
        }
    })
}

/// What the server received, in order.
pub(crate) type Log = Arc<Mutex<Vec<String>>>;

/// The thread of a fake QMP server.
pub(crate) struct Server {
    thread: std::thread::JoinHandle<()>,
    release: ExitWatch,
}

impl Server {
    /// Waits for the script to end, leaving it to wait for the client as long as it needs.
    pub(crate) fn wait(self) -> std::thread::Result<()> {
        self.thread.join()
    }

    /// For when the test is over with the client: a script still waiting for the client's end of stream is woken (a
    /// script that waits for a command it was never sent is a bug in the test), then waits for the script to end.
    pub(crate) fn join(self) -> std::thread::Result<()> {
        self.release.report_exit();
        self.thread.join()
    }
}

/// Accepts `clients` connections in turn. Each gets QEMU's greeting and `qmp_capabilities`, then `script(session)`.
///
/// Windows can lose the end of an AF_UNIX stream, so no test may end by waiting for the peer to close:
/// - A client that must see the server end is given a fake process (`fake_process`), ended once the server is done
///   (`end_process_after`).
/// - A script waits for its client with `Session::command`, which `Server::join` wakes. A script that serves more than
///   one client must not wait for the end of the first one: no `join` comes before the second.
pub(crate) fn serve(
    listener: UnixListener,
    clients: usize,
    script: impl Fn(&mut Session, &Log) + Send + 'static,
) -> (Server, Log) {
    let log: Log = Arc::default();
    let seen = log.clone();
    let release = ExitWatch::new(host::current());
    let released = release.clone();
    let thread = std::thread::spawn(move || {
        for _ in 0..clients {
            let (stream, _) = listener.accept().unwrap();
            let mut session = Session::new(stream, released.clone());
            session.send(greeting());
            let (name, _) = session.command().expect("qmp_capabilities");
            seen.lock().unwrap().push(name);
            session.reply(json!({}));
            script(&mut session, &seen);
        }
    });
    (Server { thread, release }, log)
}

fn sock() -> Sock {
    Sock::new()
}

impl Sock {
    pub(crate) fn listen(&self) -> UnixListener {
        UnixListener::bind(self.endpoint.path()).unwrap()
    }

    fn connect(&self) -> crate::Result<Qmp> {
        Qmp::connect(
            &self.endpoint,
            &ExitWatch::new(host::current()),
            &MemoryProgress::default(),
        )
    }
}

// Endpoint ============================================================================================================

#[skuld::test]
fn endpoint_names_are_unique_per_instance() {
    let temp = Path::new("temp");
    let a = QmpEndpoint::for_instance(temp, &InstanceId::random(), host::current().socket_path_max()).unwrap();
    let b = QmpEndpoint::for_instance(temp, &InstanceId::random(), host::current().socket_path_max()).unwrap();
    assert_ne!(a.path(), b.path());
    assert_ne!(a.lock_path(), b.lock_path());
    assert!(a.path().starts_with(temp));
    let name = a.path().file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("dvm-") && name.ends_with(".sock") && name.len() == 4 + 32 + 5,
        "{name}"
    );
}

#[skuld::test]
fn from_record_gives_the_same_endpoint_and_lock_as_for_instance() {
    let id = InstanceId::random();
    let made = QmpEndpoint::for_instance(Path::new("temp"), &id, host::current().socket_path_max()).unwrap();
    let recorded = QmpEndpoint::from_record(&made.path().display().to_string());
    assert_eq!(recorded, made);
    assert_eq!(recorded.lock_path(), made.lock_path());
    let mut expected = made.path().as_os_str().to_owned();
    expected.push(".lock");
    assert_eq!(made.lock_path(), PathBuf::from(expected));
}

#[skuld::test]
fn a_socket_path_over_the_given_limit_is_refused_before_launch() {
    let temp = Path::new("temp");
    let id = InstanceId::random();
    let length = temp.join(format!("dvm-{id}.sock")).to_string_lossy().len();

    assert!(
        QmpEndpoint::for_instance(temp, &id, length).is_ok(),
        "a path of exactly the limit fits"
    );
    let error = QmpEndpoint::for_instance(temp, &id, length - 1).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().contains(&format!("{}-byte", length - 1)), "{error}");

    // The limit counts UTF-8 bytes, not characters: 30 four-byte characters are 120 bytes.
    let wide = Path::new("C:\\t").join("\u{1f600}".repeat(30));
    assert!(QmpEndpoint::for_instance(&wide, &id, 107).is_err());
}

#[skuld::test]
fn the_host_limit_is_the_sun_path_length_less_its_nul() {
    // `sun_path` is 104 bytes on macOS and the BSDs, 108 on Linux and Windows.
    let expected = if cfg!(any(target_os = "macos", target_os = "freebsd")) {
        103
    } else {
        107
    };
    assert_eq!(host::current().socket_path_max(), expected);
}

// Connecting ==========================================================================================================

#[skuld::test]
fn a_path_with_a_space_and_non_ascii_works() {
    let sock = sock();
    assert!(sock.endpoint.path().to_string_lossy().contains(UNICODE_DIR));
    let (server, log) = serve(sock.listen(), 1, |session, _| {
        session.answer(json!({ "status": "running", "running": true }));
    });

    let mut qmp = sock.connect().unwrap();
    assert_eq!(qmp.query_status().unwrap().state, "running");
    drop(qmp);

    server.join().unwrap();
    assert_eq!(*log.lock().unwrap(), ["qmp_capabilities"]);
}

#[skuld::test]
fn four_sequential_clients_each_get_the_greeting() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 4, |session, _| {
        session.answer(json!({ "status": "running", "running": true }));
    });

    for _ in 0..4 {
        let mut qmp = sock.connect().unwrap();
        assert_eq!(qmp.query_status().unwrap().state, "running");
    }

    server.join().unwrap();
}

#[skuld::test]
fn connect_waits_for_the_listener() {
    let sock = sock();
    let exit = ExitWatch::new(host::current());

    std::thread::scope(|scope| {
        let client = scope.spawn(|| Qmp::connect(&sock.endpoint, &exit, &MemoryProgress::default()).map(drop));
        let (server, _) = serve(sock.listen(), 1, |_, _| {});
        client.join().unwrap().unwrap();
        server.join().unwrap();
    });
}

#[skuld::test]
fn a_machine_that_has_exited_fails_before_a_lock_file_is_touched() {
    let sock = sock();
    let exit = ExitWatch::new(host::current());
    exit.report_exit();
    let error = Qmp::connect(&sock.endpoint, &exit, &MemoryProgress::default())
        .err()
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(!sock.endpoint.lock_path().exists());
}

/// A watch whose process is a channel: it exits when the sender is dropped.
fn fake_process() -> (mpsc::Sender<()>, ExitWatch) {
    let (sender, receiver) = mpsc::channel::<()>();
    let watch = ExitWatch::watching(host::current(), move || {
        let _ = receiver.recv();
        Ok(())
    });
    (sender, watch)
}

/// Ends the fake process once `server` is done, from a thread of its own, while the calling thread reads the connection.
fn end_process_after<T>(server: Server, process: mpsc::Sender<()>, read: impl FnOnce() -> T) -> T {
    std::thread::scope(|scope| {
        scope.spawn(move || {
            server.wait().unwrap();
            drop(process);
        });
        read()
    })
}

#[skuld::test]
fn a_process_that_exits_while_connect_waits_for_the_listener_ends_the_wait() {
    let sock = sock();
    let (process, exit) = fake_process();

    let error = std::thread::scope(|scope| {
        let client = scope.spawn(|| Qmp::connect(&sock.endpoint, &exit, &MemoryProgress::default()));
        drop(process);
        client.join().unwrap().err().expect("nothing ever listens")
    });

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("exited"), "{error}");
    assert!(
        !sock.endpoint.lock_path().exists(),
        "the lock file made while waiting was left behind"
    );
}

#[skuld::test]
fn a_process_that_cannot_be_watched_fails_connect_naming_why() {
    let sock = sock();
    let exit = ExitWatch::watching(host::current(), || {
        Err(crate::Error::new(ErrorKind::Io, "the wait was refused"))
    });

    let error = Qmp::connect(&sock.endpoint, &exit, &MemoryProgress::default())
        .err()
        .expect("nothing ever listens");

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("the wait was refused"), "{error}");
}

#[skuld::test]
fn the_qmp_lock_serializes_two_processes() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |_, _| {});
    let lock = sock.endpoint.lock_path();
    let mut holder = crate::test_support::helper::command("hold-lock", &[&lock.display().to_string()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(holder.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "held");

    let (progress, messages) = ChannelProgress::new();
    std::thread::scope(|scope| {
        let client =
            scope.spawn(|| Qmp::connect(&sock.endpoint, &ExitWatch::new(host::current()), &*progress).map(drop));
        let message = messages.recv().unwrap();
        assert!(message.starts_with("waiting for "), "{message}");
        // Releasing the lock is what lets the second connect proceed.
        drop(holder.stdin.take());
        client.join().unwrap().unwrap();
    });

    holder.wait().unwrap();
    server.join().unwrap();
}

// Commands ============================================================================================================

#[skuld::test]
fn forwarded_ports_asks_info_usernet_and_parses_the_answer() {
    let sock = sock();
    let (server, log) = serve(sock.listen(), 1, |session, log| {
        let (name, arguments) = session.command().unwrap();
        log.lock()
            .unwrap()
            .push(format!("{name} {}", arguments["command-line"].as_str().unwrap()));
        session.reply(json!(
            "Hub 0 (#net0):\r\n  Protocol[State]    FD  Source Address  Port   Dest. Address  Port RecvQ SendQ\r\n  \
             TCP[HOST_FORWARD]  258       127.0.0.1 49986       10.0.2.15    22     0     0\r\n"
        ));
    });

    let forwards = sock.connect().unwrap().forwarded_ports().unwrap();

    assert_eq!(forwards, [Forward { host: 49986, guest: 22 }]);
    server.join().unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        ["qmp_capabilities", "human-monitor-command info usernet"]
    );
}

#[skuld::test]
fn an_error_reply_is_an_error_naming_the_command() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.command().unwrap();
        session.fail("the guest said no");
    });

    let error = sock.connect().unwrap().stop().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(
        error.message().contains("stop") && error.message().contains("the guest said no"),
        "{error}"
    );
    server.join().unwrap();
}

#[skuld::test]
fn an_io_error_run_state_is_reported_as_io_error() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.answer(json!({ "status": "io-error", "running": false }));
    });

    let status = sock.connect().unwrap().query_status().unwrap();

    assert_eq!(status.state, "io-error");
    assert!(!status.is_settled_for_powerdown());
    server.join().unwrap();
}

#[skuld::test]
fn target_arch_maps_the_qemu_target_to_an_architecture() {
    for (target, arch) in [("x86_64", crate::Arch::Amd64), ("aarch64", crate::Arch::Arm64)] {
        let sock = sock();
        let (server, _) = serve(sock.listen(), 1, move |session, _| {
            session.answer(json!({ "arch": target }));
        });
        assert_eq!(sock.connect().unwrap().target_arch().unwrap(), arch);
        server.join().unwrap();
    }

    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.answer(json!({ "arch": "riscv64" }));
    });
    let error = sock.connect().unwrap().target_arch().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().contains("riscv64"), "{error}");
    server.join().unwrap();
}

#[skuld::test]
fn events_devvm_does_not_read_are_skipped_wherever_they_arrive() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.event("RTC_CHANGE", Some(json!({ "offset": 0 })));
        session.event("AN_EVENT_QEMU_HAS_NOT_INVENTED_YET", None);
        let (name, _) = session.command().unwrap();
        assert_eq!(name, "cont");
        session.event("RTC_CHANGE", Some(json!({ "offset": 1 })));
        session.reply(json!({}));
    });

    sock.connect().unwrap().cont().unwrap();

    server.join().unwrap();
}

#[skuld::test]
fn an_event_devvm_reads_that_does_not_fit_the_schema_is_an_error_naming_it() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.event(
            "SHUTDOWN",
            Some(json!({ "guest": "not a bool", "reason": "host-qmp-quit" })),
        );
        while session.command().is_some() {}
    });

    let error = sock.connect().unwrap().cont().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(error.message().contains("SHUTDOWN"), "{error}");
    server.join().unwrap();
}

#[skuld::test]
fn shutdown_of_a_vm_that_is_already_paused_fails_naming_the_state_and_sends_no_powerdown() {
    let sock = sock();
    let (server, log) = serve(sock.listen(), 1, |session, log| {
        let name = session.answer(json!({ "status": "paused", "running": false }));
        log.lock().unwrap().push(name);
        // Anything further the client sent would be logged here.
        while let Some((name, _)) = session.command() {
            log.lock().unwrap().push(name);
            session.reply(json!({}));
        }
    });
    let serial = Path::new("serial.log");

    let mut qmp = sock.connect().unwrap();
    let error = qmp
        .require_state(&["running", "suspended", "shutdown"], "shut down the VM", serial)
        .unwrap_err();
    drop(qmp);

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(
        error.message().contains("paused") && error.message().contains("serial.log"),
        "{error}"
    );
    server.join().unwrap();
    assert_eq!(*log.lock().unwrap(), ["qmp_capabilities", "query-status"]);
}

#[skuld::test]
fn a_state_in_the_acceptable_list_passes() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.answer(json!({ "status": "suspended", "running": false }));
    });

    sock.connect()
        .unwrap()
        .require_state(&["running", "suspended"], "shut down the VM", Path::new("serial.log"))
        .unwrap();

    server.join().unwrap();
}

// system_reset --------------------------------------------------------------------------------------------------------

#[skuld::test]
fn system_reset_returns_after_the_reset_event() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.command().unwrap();
        session.reply(json!({}));
        session.event(
            "RESET",
            Some(json!({ "guest": false, "reason": "host-qmp-system-reset" })),
        );
        // Held open: returning must not depend on the connection ending.
        while session.command().is_some() {}
    });

    sock.connect().unwrap().system_reset().unwrap();

    server.join().unwrap();
}

#[skuld::test]
fn system_reset_accepts_the_reset_event_ahead_of_the_reply() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.command().unwrap();
        session.event(
            "RESET",
            Some(json!({ "guest": false, "reason": "host-qmp-system-reset" })),
        );
        session.reply(json!({}));
    });

    sock.connect().unwrap().system_reset().unwrap();

    server.join().unwrap();
}

#[skuld::test]
fn system_reset_is_not_done_by_a_reset_the_guest_did_by_itself() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.command().unwrap();
        session.reply(json!({}));
        session.event("RESET", Some(json!({ "guest": true, "reason": "guest-reset" })));
        // Then a line that cannot be read, which ends the call if the guest's own reset did not.
        session.send_raw(b"\xff\xfe\n");
        while session.command().is_some() {}
    });

    let mut qmp = sock.connect().unwrap();
    let error = qmp.system_reset().unwrap_err();
    drop(qmp);

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    server.join().unwrap();
}

#[skuld::test]
fn system_reset_is_not_a_no_reboot_error_on_a_shutdown_the_guest_did_by_itself() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.command().unwrap();
        session.reply(json!({}));
        session.event("SHUTDOWN", Some(json!({ "guest": true, "reason": "guest-shutdown" })));
        session.event(
            "RESET",
            Some(json!({ "guest": false, "reason": "host-qmp-system-reset" })),
        );
        while session.command().is_some() {}
    });

    sock.connect().unwrap().system_reset().unwrap();

    server.join().unwrap();
}

#[skuld::test]
fn system_reset_is_not_done_on_the_reply_alone() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.command().unwrap();
        session.reply(json!({}));
        // Nothing but the reply, then a line that cannot be read: what ends the call is the stream, in order, and not
        // an end of stream that Windows may lose.
        session.send_raw(b"\xff\xfe\n");
        while session.command().is_some() {}
    });

    let mut qmp = sock.connect().unwrap();
    let error = qmp.system_reset().unwrap_err();
    drop(qmp);

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    server.join().unwrap();
}

#[skuld::test]
fn system_reset_of_a_qemu_that_exits_before_the_reset_event_is_a_lost_connection() {
    let sock = sock();
    let (process, exit) = fake_process();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.command().unwrap();
        session.reply(json!({}));
        // No RESET event, and the server is done.
    });

    let mut qmp = Qmp::connect(&sock.endpoint, &exit, &MemoryProgress::default()).unwrap();
    let error = end_process_after(server, process, || qmp.system_reset().unwrap_err());

    // The exit can come before the reply is written: either way the call did not finish and names what it waited for.
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("system_reset"), "{error}");
}

#[skuld::test]
fn system_reset_of_a_no_reboot_qemu_is_an_error_naming_no_reboot() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.command().unwrap();
        session.reply(json!({}));
        session.event(
            "SHUTDOWN",
            Some(json!({ "guest": false, "reason": "host-qmp-system-reset" })),
        );
    });

    let error = sock.connect().unwrap().system_reset().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("no-reboot"), "{error}");
    server.join().unwrap();
}

// An exit while the client has bytes it has not read ------------------------------------------------------------------

/// A connection to a server that wrote `lines` on its own right after the handshake and then holds the connection open
/// (a stream whose end Windows may lose). Returns once the server has written them.
fn connected_to_a_server_that_wrote(
    sock: &Sock,
    exit: &ExitWatch,
    lines: impl Fn(&mut Session) + Send + 'static,
) -> (Server, Qmp) {
    let (written, written_rx) = mpsc::channel::<()>();
    let (server, _) = serve(sock.listen(), 1, move |session, _| {
        lines(session);
        written.send(()).unwrap();
        while session.command().is_some() {}
    });
    let qmp = Qmp::connect(&sock.endpoint, exit, &MemoryProgress::default()).unwrap();
    written_rx.recv().unwrap();
    (server, qmp)
}

// macOS flushes unread bytes when `shutdown(Read)` wakes a reader (measured in CI), so `UnsupportedHost` cannot keep them
// there; Windows and Linux keep them.
#[cfg(not(target_vendor = "apple"))]
#[skuld::test]
fn a_reply_that_arrived_before_the_exit_is_read_after_it() {
    let sock = sock();
    let exit = ExitWatch::new(host::current());
    let (server, mut qmp) = connected_to_a_server_that_wrote(&sock, &exit, |session| session.reply(json!({})));
    exit.report_exit();

    // `quit`'s reply is the one QEMU writes just before it exits.
    qmp.quit().unwrap();

    drop(qmp);
    server.join().unwrap();
}

// macOS flushes unread bytes when `shutdown(Read)` wakes a reader (measured in CI), so `UnsupportedHost` cannot keep them
// there; Windows and Linux keep them.
#[cfg(not(target_vendor = "apple"))]
#[skuld::test]
fn a_no_reboot_shutdown_that_arrived_before_the_exit_is_still_read() {
    let sock = sock();
    let exit = ExitWatch::new(host::current());
    let (server, mut qmp) = connected_to_a_server_that_wrote(&sock, &exit, |session| {
        session.reply(json!({}));
        session.event(
            "SHUTDOWN",
            Some(json!({ "guest": false, "reason": "host-qmp-system-reset" })),
        );
    });
    exit.report_exit();

    let error = qmp.system_reset().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("no-reboot"), "{error}");
    drop(qmp);
    server.join().unwrap();
}

#[skuld::test]
fn the_read_after_the_last_buffered_event_of_an_exited_process_ends_instead_of_blocking() {
    let sock = sock();
    let exit = ExitWatch::new(host::current());
    let (server, mut qmp) = connected_to_a_server_that_wrote(&sock, &exit, |session| session.event("POWERDOWN", None));
    exit.report_exit();

    // The server still holds the connection open: only the exit can end this wait.
    assert_eq!(qmp.wait_for_end().unwrap(), VmEnd::Exited);

    drop(qmp);
    server.join().unwrap();
}

// wait_for_end --------------------------------------------------------------------------------------------------------

#[skuld::test]
fn wait_for_end_returns_exited_when_the_server_closes_after_shutdown() {
    let sock = sock();
    let (process, exit) = fake_process();
    let (acked, acked_rx) = mpsc::channel::<()>();
    let (server, _) = serve(sock.listen(), 1, move |session, _| {
        session.answer(json!({}));
        session.event("POWERDOWN", None);
        session.event("SHUTDOWN", Some(json!({ "guest": true, "reason": "guest-shutdown" })));
        // The connection closes once the client has its reply.
        let _ = acked_rx.recv();
    });

    let mut qmp = Qmp::connect(&sock.endpoint, &exit, &MemoryProgress::default()).unwrap();
    qmp.system_powerdown().unwrap();
    acked.send(()).unwrap();
    let end = end_process_after(server, process, || qmp.wait_for_end());

    assert_eq!(end.unwrap(), VmEnd::Exited);
}

#[skuld::test]
fn wait_for_end_reports_a_stop_event_as_paused_with_the_run_state() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.event("STOP", None);
        session.answer(json!({ "status": "io-error", "running": false }));
        // The client has what it needs; it ends the session by dropping it.
        while session.command().is_some() {}
    });

    let mut qmp = sock.connect().unwrap();
    assert_eq!(qmp.wait_for_end().unwrap(), VmEnd::Paused("io-error".to_owned()));
    drop(qmp);

    server.join().unwrap();
}

#[skuld::test]
fn a_stop_event_in_a_normal_state_is_checked_and_does_not_end_the_wait() {
    let sock = sock();
    let (server, log) = serve(sock.listen(), 1, |session, log| {
        // A guest that shut itself down stops: not a pause. The second `STOP` can be read only if the wait went on after
        // the first, and it is a real pause.
        for status in ["shutdown", "io-error"] {
            session.event("STOP", None);
            let name = session.answer(json!({ "status": status, "running": false }));
            log.lock().unwrap().push(name);
        }
        while session.command().is_some() {}
    });

    let mut qmp = sock.connect().unwrap();
    let end = qmp.wait_for_end().unwrap();
    drop(qmp);

    assert_eq!(end, VmEnd::Paused("io-error".to_owned()));
    server.join().unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        ["qmp_capabilities", "query-status", "query-status"]
    );
}

// A QEMU that goes away, or a socket that fails, at the wrong moment --------------------------------------------------

/// Windows can lose the end of the stream when a server writes and closes, so these tests end the fake
/// process once the server is done: a lost end of stream is woken by the exit watch instead of hanging the test.
#[skuld::test]
fn a_server_that_closes_after_the_greeting_is_a_lost_connection() {
    let sock = sock();
    let (process, exit) = fake_process();
    let listener = sock.listen();
    let release = ExitWatch::new(host::current());
    let thread = std::thread::spawn({
        let release = release.clone();
        move || {
            let (stream, _) = listener.accept().unwrap();
            let mut session = Session::new(stream, release);
            session.send(greeting());
            // Dropped before `qmp_capabilities` is answered.
        }
    });
    let server = Server { thread, release };

    let error = end_process_after(server, process, || {
        Qmp::connect(&sock.endpoint, &exit, &MemoryProgress::default())
            .err()
            .expect("the handshake cannot finish")
    });

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
}

#[skuld::test]
fn a_server_that_closes_after_query_status_loses_the_next_command() {
    let sock = sock();
    let (process, exit) = fake_process();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.answer(json!({ "status": "running", "running": true }));
    });

    let mut qmp = Qmp::connect(&sock.endpoint, &exit, &MemoryProgress::default()).unwrap();
    assert_eq!(qmp.query_status().unwrap().state, "running");
    server.join().unwrap();
    drop(process);
    let error = qmp.system_powerdown().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
}

#[skuld::test]
fn a_process_that_exits_while_a_read_is_blocked_ends_the_read() {
    let sock = sock();
    let (process, exit) = fake_process();
    let (asked, asked_rx) = mpsc::channel();
    let (finished, finished_rx) = mpsc::channel::<()>();
    // The server takes the command and never answers; it keeps the connection open until the client is done, so the
    // read can end only because the process exited.
    let (server, _) = serve(sock.listen(), 1, move |session, _| {
        let (name, _) = session.command().unwrap();
        asked.send(name).unwrap();
        let _ = finished_rx.recv();
    });
    let mut qmp = Qmp::connect(&sock.endpoint, &exit, &MemoryProgress::default()).unwrap();

    let error = std::thread::scope(|scope| {
        let client = scope.spawn(move || qmp.query_status());
        assert_eq!(asked_rx.recv().unwrap(), "query-status");
        drop(process);
        client.join().unwrap().unwrap_err()
    });

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    finished.send(()).unwrap();
    server.join().unwrap();
}

#[skuld::test]
fn a_read_that_starts_after_the_process_exited_ends_at_once() {
    let sock = sock();
    let exit = ExitWatch::new(host::current());
    let (finished, finished_rx) = mpsc::channel::<()>();
    // The server keeps the connection open and says nothing, so a read can end only because the process exited.
    let (server, _) = serve(sock.listen(), 1, move |_, _| {
        let _ = finished_rx.recv();
    });
    let mut qmp = Qmp::connect(&sock.endpoint, &exit, &MemoryProgress::default()).unwrap();

    // The exit comes before the first read, so there is nothing blocked for the interrupt to wake.
    exit.report_exit();
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
    let exit = ExitWatch::new(host::current());
    let (finished, finished_rx) = mpsc::channel::<()>();
    let (server, _) = serve(sock.listen(), 1, move |_, _| {
        let _ = finished_rx.recv();
    });
    let mut qmp = Qmp::connect(&sock.endpoint, &exit, &MemoryProgress::default()).unwrap();

    exit.report_exit();
    let error = qmp.stop().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    finished.send(()).unwrap();
    server.join().unwrap();
}

/// Not a socket test: Windows' AF_UNIX sockets sometimes never deliver the end of the stream to a reader after the peer
/// wrote and closed (measured, minimal program: about one connection in two thousand, with a shutdown before the close
/// as well), so a fake server that dies mid-line would hang the test run.
#[skuld::test]
fn a_line_cut_off_before_its_newline_is_the_end_of_the_stream() {
    use super::is_whole_line;

    assert!(!is_whole_line(0, ""), "nothing was read");
    assert!(!is_whole_line(17, r#"{"return": {"stat"#), "QEMU died mid-write");
    assert!(is_whole_line(3, "{}\n"));
}

#[skuld::test]
fn a_line_that_is_not_text_is_an_io_error_and_not_a_lost_connection() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.command().unwrap();
        session.send_raw(b"\xff\xfe\n");
        // The peer stays up: the process behind it is alive.
        while session.command().is_some() {}
    });

    let mut qmp = sock.connect().unwrap();
    let error = qmp.query_status().unwrap_err();
    drop(qmp);

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    server.join().unwrap();
}

#[skuld::test]
fn only_a_closed_stream_means_the_peer_is_gone() {
    use super::{is_closed, is_not_listening};
    use std::io::{Error, ErrorKind::*};

    for kind in [ConnectionReset, ConnectionAborted, BrokenPipe] {
        assert!(is_closed(&Error::from(kind)), "{kind:?}");
    }
    for kind in [InvalidData, PermissionDenied, WouldBlock, TimedOut, Other] {
        assert!(!is_closed(&Error::from(kind)), "{kind:?}");
    }

    // At connect time a peer that is still starting up refuses, or resets or aborts the attempt.
    for kind in [NotFound, ConnectionRefused, ConnectionReset, ConnectionAborted] {
        assert!(is_not_listening(&Error::from(kind)), "{kind:?}");
    }
    for kind in [PermissionDenied, InvalidData, InvalidInput] {
        assert!(!is_not_listening(&Error::from(kind)), "{kind:?}");
    }
}
