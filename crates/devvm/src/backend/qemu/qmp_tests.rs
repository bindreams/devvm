use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::{Qmp, QmpEndpoint, VmEnd};
use crate::ErrorKind;
use crate::backend::qemu::ports::Forward;
use crate::backend::qemu::uds::UnixListener;
use crate::host::{self, Link, ProcessWatch};
use crate::instance::record::InstanceId;
use crate::test_support::fixtures::{ChannelProgress, MemoryProgress};
use crate::test_support::manual_exit::ManualExit;
use crate::test_support::qemu::{Sock, assert_unicode_socket_path, socket_dir};
use crate::test_support::refusing::{REFUSED, Refusing};

// A fake QMP server ===================================================================================================

/// One accepted connection, speaking QMP as QEMU does.
pub(crate) struct Session {
    link: BufReader<Box<dyn Link>>,
}

impl Session {
    /// `release` is the process this server stands for: when the test ends it (`Server::join`), a read that waits for
    /// the client's end of stream returns even if Windows lost that end of stream.
    fn new(stream: crate::backend::qemu::uds::UnixStream, release: &Arc<dyn ProcessWatch>) -> Session {
        Session {
            link: BufReader::new(release.link(stream).unwrap()),
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
        let link = self.link.get_mut();
        let _ = link.write_all(bytes).and_then(|()| link.flush());
    }

    /// The next command as `(name, arguments)`; `None` when the client closed the connection or the test released the
    /// server (`Server::join`).
    pub(crate) fn command(&mut self) -> Option<(String, Value)> {
        let mut line = String::new();
        if self.link.read_line(&mut line).ok()? == 0 {
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

    pub(crate) fn event(&mut self, name: &str, data: Option<Value>) {
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
    release: ManualExit,
}

impl Server {
    /// Waits for the script to end, leaving it to wait for the client as long as it needs.
    pub(crate) fn wait(self) -> std::thread::Result<()> {
        self.thread.join()
    }

    /// For when the test is over with the client: a script still waiting for the client's end of stream is woken (a
    /// script that waits for a command it was never sent is a bug in the test), then waits for the script to end.
    pub(crate) fn join(self) -> std::thread::Result<()> {
        self.release.exit();
        self.thread.join()
    }
}

/// Accepts `clients` connections in turn. Each gets QEMU's greeting and `qmp_capabilities`, then `script(session)`.
///
/// Windows can lose the end of an AF_UNIX stream, so no test may end by waiting for the peer to close:
/// - A client that must see the server end is given a `ManualExit` process, ended once the server is done
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
    let release = ManualExit::new();
    let released = release.watch();
    let thread = std::thread::spawn(move || {
        for _ in 0..clients {
            let (stream, _) = listener.accept().unwrap();
            let mut session = Session::new(stream, &released);
            session.send(greeting());
            let (name, _) = session.command().expect("qmp_capabilities");
            seen.lock().unwrap().push(name);
            session.reply(json!({}));
            script(&mut session, &seen);
        }
    });
    (Server { thread, release }, log)
}

pub(crate) fn sock() -> Sock {
    Sock::new()
}

impl Sock {
    pub(crate) fn listen(&self) -> UnixListener {
        UnixListener::bind(self.endpoint.path()).unwrap()
    }

    pub(crate) fn connect(&self) -> crate::Result<Qmp> {
        Qmp::connect(&self.endpoint, &ManualExit::new().watch(), &MemoryProgress::default())
    }
}

// Endpoint ============================================================================================================

#[skuld::test]
fn two_launches_of_one_instance_get_different_socket_and_lock_paths() {
    let temp = Path::new("temp");
    let id = InstanceId::random();
    let a = QmpEndpoint::for_launch(temp, &id, host::current().socket_path_max()).unwrap();
    let b = QmpEndpoint::for_launch(temp, &id, host::current().socket_path_max()).unwrap();
    assert_ne!(a.path(), b.path());
    assert_ne!(a.lock_path(), b.lock_path());
    assert!(a.path().starts_with(temp));
    for endpoint in [&a, &b] {
        let name = endpoint.path().file_name().unwrap().to_string_lossy().into_owned();
        let launch = name
            .strip_prefix(&format!("dvm-{id}-"))
            .and_then(|rest| rest.strip_suffix(".sock"));
        assert!(
            launch.is_some_and(|hex| hex.len() == 8 && hex.bytes().all(|b| b.is_ascii_hexdigit())),
            "{name}"
        );
    }
}

#[skuld::test]
fn endpoints_of_different_instances_differ() {
    let temp = Path::new("temp");
    let a = QmpEndpoint::for_launch(temp, &InstanceId::random(), host::current().socket_path_max()).unwrap();
    let b = QmpEndpoint::for_launch(temp, &InstanceId::random(), host::current().socket_path_max()).unwrap();
    assert_ne!(a.path(), b.path());
}

#[skuld::test]
fn from_record_gives_the_same_endpoint_and_lock_as_for_launch() {
    let id = InstanceId::random();
    let made = QmpEndpoint::for_launch(Path::new("temp"), &id, host::current().socket_path_max()).unwrap();
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
    // Every launch's path has the same length, so the length of one is the exact limit of the next.
    let length = QmpEndpoint::for_launch(temp, &id, usize::MAX)
        .unwrap()
        .path()
        .to_string_lossy()
        .len();

    assert!(
        QmpEndpoint::for_launch(temp, &id, length).is_ok(),
        "a path of exactly the limit fits"
    );
    let error = QmpEndpoint::for_launch(temp, &id, length - 1).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().contains(&format!("{}-byte", length - 1)), "{error}");

    // The limit counts UTF-8 bytes, not characters: 30 four-byte characters are 120 bytes.
    let wide = Path::new("C:\\t").join("\u{1f600}".repeat(30));
    assert!(QmpEndpoint::for_launch(&wide, &id, 107).is_err());
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
    assert_unicode_socket_path(sock.endpoint.path());
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
    let exit = ManualExit::new().watch();

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
    let exit = ManualExit::new();
    exit.exit();
    let error = Qmp::connect(&sock.endpoint, &exit.watch(), &MemoryProgress::default())
        .err()
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(!sock.endpoint.lock_path().exists());
}

/// Ends `process` once `server` is done, from a thread of its own, while the calling thread reads the connection.
fn end_process_after<T>(server: Server, process: &ManualExit, read: impl FnOnce() -> T) -> T {
    std::thread::scope(|scope| {
        scope.spawn(move || {
            server.wait().unwrap();
            process.exit();
        });
        read()
    })
}

#[skuld::test]
fn a_process_that_exits_while_connect_waits_for_the_listener_ends_the_wait() {
    let sock = sock();
    let process = ManualExit::new();

    let error = std::thread::scope(|scope| {
        let client = scope.spawn(|| Qmp::connect(&sock.endpoint, &process.watch(), &MemoryProgress::default()));
        process.exit();
        client.join().unwrap().err().expect("nothing ever listens")
    });

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("exited"), "{error}");
}

#[skuld::test]
fn a_peer_that_closes_before_the_greeting_fails_as_a_lost_connection_and_keeps_the_lock_file() {
    let sock = sock();
    let process = ManualExit::new();
    let listener = sock.listen();
    let error = std::thread::scope(|scope| {
        let server = scope.spawn(|| {
            let (stream, _) = listener.accept().unwrap();
            // QEMU closes the socket during teardown, and the process object signals after that.
            drop(stream);
            process.exit();
        });
        let error = Qmp::connect(&sock.endpoint, &process.watch(), &MemoryProgress::default())
            .err()
            .expect("QEMU never greeted");
        server.join().unwrap();
        error
    });

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(
        sock.endpoint.lock_path().exists(),
        "connect deleted the lock file: only launch and the machine's removal may"
    );
}

#[skuld::test]
fn a_failed_handshake_keeps_the_lock_file() {
    let sock = sock();
    let release = ManualExit::new();
    let listener = sock.listen();
    let thread = std::thread::spawn({
        let released = release.watch();
        move || {
            let (stream, _) = listener.accept().unwrap();
            let mut session = Session::new(stream, &released);
            session.send(json!({ "hello": 1 }));
            // Held until the client has failed: the process lives, and so does the connection.
            session.command();
        }
    });
    let server = Server { thread, release };

    let error = sock.connect().err().expect("the greeting is not QMP");

    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(
        sock.endpoint.lock_path().exists(),
        "connect deleted the lock file: only launch and the machine's removal may"
    );
    server.join().unwrap();
}

#[skuld::test]
fn a_process_that_cannot_be_asked_fails_connect_with_the_refusal_before_a_lock_file_is_touched() {
    let sock = sock();
    let watch = Refusing::new(ManualExit::new().watch());
    watch.refuse_has_exited();

    let error = Qmp::connect(&sock.endpoint, &watch.watch(), &MemoryProgress::default())
        .err()
        .unwrap();

    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert_eq!(error.message(), REFUSED);
    assert!(!sock.endpoint.lock_path().exists());
}

#[skuld::test]
fn a_process_that_cannot_be_paused_on_fails_the_wait_for_the_listener_with_the_refusal() {
    let sock = sock();
    let watch = Refusing::new(ManualExit::new().watch());
    watch.refuse_pause();

    // Nothing listens, so connect must wait, and the wait is what refuses.
    let error = Qmp::connect(&sock.endpoint, &watch.watch(), &MemoryProgress::default())
        .err()
        .unwrap();

    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert_eq!(error.message(), REFUSED);
}

#[skuld::test]
fn a_process_watch_that_cannot_watch_the_connection_fails_connect_with_an_io_error_naming_the_socket() {
    let sock = sock();
    let watch = Refusing::new(ManualExit::new().watch());
    watch.refuse_link();
    // The connection is made by the kernel's backlog; nothing accepts it, and nothing needs to.
    let _listener = sock.listen();

    let error = Qmp::connect(&sock.endpoint, &watch.watch(), &MemoryProgress::default())
        .err()
        .unwrap();

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("cannot watch the connection to"), "{error}");
    assert!(
        error.message().contains(&sock.endpoint.path().display().to_string()),
        "{error}"
    );
}

#[skuld::test]
fn a_connect_error_that_is_not_a_missing_listener_is_an_io_error_naming_the_socket() {
    // A path over the host's limit can never connect, so waiting for a listener would be for ever. The lock file's path
    // is a plain file path and stays valid.
    let long = socket_dir().join("x".repeat(host::current().socket_path_max()));
    let endpoint = QmpEndpoint::from_record(&long.display().to_string());
    let process = ManualExit::new();

    let error = Qmp::connect(&endpoint, &process.watch(), &MemoryProgress::default())
        .err()
        .unwrap();
    let _ = std::fs::remove_file(endpoint.lock_path());

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(
        error.message().contains("cannot connect to the QEMU control socket"),
        "{error}"
    );
}

#[skuld::test]
fn a_read_error_while_the_process_cannot_be_asked_is_reported_and_not_taken_for_the_end() {
    let sock = sock();
    let watch = Refusing::new(ManualExit::new().watch());
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        // QEMU is alive: the connection stays up until the client lets go.
        while session.command().is_some() {}
    });
    let mut qmp = Qmp::connect(&sock.endpoint, &watch.watch(), &MemoryProgress::default()).unwrap();
    watch.fail_reads_with(std::io::ErrorKind::PermissionDenied);
    watch.refuse_has_exited();

    let error = qmp.wait_for_end().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    drop(qmp);
    server.join().unwrap();
}

#[skuld::test]
fn a_corrupt_line_is_reported_as_itself_even_when_the_process_has_exited_since() {
    let sock = sock();
    let process = Arc::new(ManualExit::new());
    let (server, _) = serve(sock.listen(), 1, {
        let process = process.clone();
        move |session, _| {
            session.send_raw(b"\xff\xfe\n");
            process.exit();
        }
    });
    let mut qmp = Qmp::connect(&sock.endpoint, &process.watch(), &MemoryProgress::default()).unwrap();
    server.wait().unwrap();

    let error = qmp.wait_for_end().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    drop(qmp);
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
        let client = scope.spawn(|| Qmp::connect(&sock.endpoint, &ManualExit::new().watch(), &*progress).map(drop));
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
    let process = ManualExit::new();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.command().unwrap();
        session.reply(json!({}));
        // No RESET event, and the server is done.
    });

    let mut qmp = Qmp::connect(&sock.endpoint, &process.watch(), &MemoryProgress::default()).unwrap();
    let error = end_process_after(server, &process, || qmp.system_reset().unwrap_err());

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

// wait_for_end --------------------------------------------------------------------------------------------------------

#[skuld::test]
fn wait_for_end_returns_exited_when_the_server_closes_after_shutdown() {
    let sock = sock();
    let process = ManualExit::new();
    let (acked, acked_rx) = mpsc::channel::<()>();
    let (server, _) = serve(sock.listen(), 1, move |session, _| {
        session.answer(json!({}));
        session.event("POWERDOWN", None);
        session.event("SHUTDOWN", Some(json!({ "guest": true, "reason": "guest-shutdown" })));
        // The connection closes once the client has its reply.
        let _ = acked_rx.recv();
    });

    let mut qmp = Qmp::connect(&sock.endpoint, &process.watch(), &MemoryProgress::default()).unwrap();
    qmp.system_powerdown().unwrap();
    acked.send(()).unwrap();
    let end = end_process_after(server, &process, || qmp.wait_for_end());

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

/// Windows can lose the end of the stream when a server writes and closes, so these tests end the process once the
/// server is done: the exit ends the read instead of the test hanging.
#[skuld::test]
fn a_server_that_closes_after_the_greeting_is_a_lost_connection() {
    let sock = sock();
    let process = ManualExit::new();
    let listener = sock.listen();
    let release = ManualExit::new();
    let thread = std::thread::spawn({
        let released = release.watch();
        move || {
            let (stream, _) = listener.accept().unwrap();
            let mut session = Session::new(stream, &released);
            session.send(greeting());
            // Dropped before `qmp_capabilities` is answered.
        }
    });
    let server = Server { thread, release };

    let error = end_process_after(server, &process, || {
        Qmp::connect(&sock.endpoint, &process.watch(), &MemoryProgress::default())
            .err()
            .expect("the handshake cannot finish")
    });

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
}

#[skuld::test]
fn a_server_that_closes_after_query_status_loses_the_next_command() {
    let sock = sock();
    let process = ManualExit::new();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.answer(json!({ "status": "running", "running": true }));
    });

    let mut qmp = Qmp::connect(&sock.endpoint, &process.watch(), &MemoryProgress::default()).unwrap();
    assert_eq!(qmp.query_status().unwrap().state, "running");
    server.join().unwrap();
    process.exit();
    let error = qmp.system_powerdown().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
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
