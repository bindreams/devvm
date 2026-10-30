use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::{Qmp, QmpEndpoint, VmEnd};
use crate::ErrorKind;
use crate::backend::qemu::ports::Forward;
use crate::backend::qemu::uds::{UnixListener, UnixStream};
use crate::instance::record::InstanceId;
use crate::test_support::fixtures::{ChannelProgress, MemoryProgress, UNICODE_DIR};
use crate::test_support::qemu::Sock;

// A fake QMP server ===================================================================================================

/// One accepted connection, speaking QMP as QEMU does.
pub(crate) struct Session {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Session {
    fn new(stream: UnixStream) -> Session {
        Session {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
        }
    }

    fn send(&mut self, value: Value) {
        let mut line = value.to_string();
        line.push('\n');
        // The client may be gone already; a script that cares reads first.
        let _ = self
            .writer
            .write_all(line.as_bytes())
            .and_then(|()| self.writer.flush());
    }

    /// The next command as `(name, arguments)`; `None` when the client closed the connection.
    fn command(&mut self) -> Option<(String, Value)> {
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

    fn reply(&mut self, value: Value) {
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
    fn answer(&mut self, value: Value) -> String {
        let (name, _) = self.command().expect("a command was expected");
        self.reply(value);
        name
    }
}

/// What the server received, in order.
type Log = Arc<Mutex<Vec<String>>>;

/// Accepts `clients` connections in turn. Each gets QEMU's greeting and `qmp_capabilities`, then `script(session)`.
fn serve(
    listener: UnixListener,
    clients: usize,
    script: impl Fn(&mut Session, &Log) + Send + 'static,
) -> (std::thread::JoinHandle<()>, Log) {
    let log: Log = Arc::default();
    let seen = log.clone();
    let handle = std::thread::spawn(move || {
        for _ in 0..clients {
            let (stream, _) = listener.accept().unwrap();
            let mut session = Session::new(stream);
            session.send(json!({
                "QMP": {
                    "version": { "qemu": { "micro": 2, "minor": 1, "major": 11 }, "package": "v11.1.2" },
                    "capabilities": ["oob"]
                }
            }));
            let (name, _) = session.command().expect("qmp_capabilities");
            seen.lock().unwrap().push(name);
            session.reply(json!({}));
            script(&mut session, &seen);
        }
    });
    (handle, log)
}

fn sock() -> Sock {
    Sock::new()
}

impl Sock {
    fn listen(&self) -> UnixListener {
        UnixListener::bind(self.endpoint.path()).unwrap()
    }

    fn connect(&self) -> crate::Result<Qmp> {
        Qmp::connect(&self.endpoint, &|| true, &MemoryProgress::default())
    }
}

// Endpoint ============================================================================================================

#[skuld::test]
fn endpoint_names_are_unique_per_instance() {
    let temp = Path::new("temp");
    let a = QmpEndpoint::for_instance(temp, &InstanceId::random()).unwrap();
    let b = QmpEndpoint::for_instance(temp, &InstanceId::random()).unwrap();
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
    let made = QmpEndpoint::for_instance(Path::new("temp"), &id).unwrap();
    let recorded = QmpEndpoint::from_record(&made.path().display().to_string());
    assert_eq!(recorded, made);
    assert_eq!(recorded.lock_path(), made.lock_path());
    let mut expected = made.path().as_os_str().to_owned();
    expected.push(".lock");
    assert_eq!(made.lock_path(), PathBuf::from(expected));
}

#[skuld::test]
fn a_socket_path_over_the_sun_path_limit_is_refused_before_launch() {
    let long = std::env::temp_dir().join("x".repeat(120));
    let error = QmpEndpoint::for_instance(&long, &InstanceId::random()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().contains("107"), "{error}");

    // The limit counts UTF-8 bytes, not characters: 30 four-byte characters are 120 bytes.
    let wide = Path::new("C:\\t").join("\u{1f600}".repeat(30));
    assert!(QmpEndpoint::for_instance(&wide, &InstanceId::random()).is_err());
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

/// A liveness predicate that tells the test each time it is asked.
fn watched(alive: &Arc<AtomicBool>) -> (Box<dyn Fn() -> bool + Send + Sync>, mpsc::Receiver<()>) {
    let (sender, receiver) = mpsc::channel();
    let sender = Mutex::new(sender);
    let alive = alive.clone();
    (
        Box::new(move || {
            let _ = sender.lock().unwrap().send(());
            alive.load(Ordering::SeqCst)
        }),
        receiver,
    )
}

#[skuld::test]
fn connect_gives_up_when_the_vm_dies_while_nothing_listens() {
    let sock = sock();
    let alive = Arc::new(AtomicBool::new(true));
    let (predicate, asked) = watched(&alive);

    let error = std::thread::scope(|scope| {
        let client = scope.spawn(|| Qmp::connect(&sock.endpoint, &predicate, &MemoryProgress::default()));
        // Once for the entry check and once after the first refused attempt: the loop is running.
        asked.recv().unwrap();
        asked.recv().unwrap();
        alive.store(false, Ordering::SeqCst);
        client
            .join()
            .unwrap()
            .err()
            .expect("connect must fail once the VM is dead")
    });

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("exited"), "{error}");
}

#[skuld::test]
fn connect_waits_for_the_listener() {
    let sock = sock();
    let alive = Arc::new(AtomicBool::new(true));
    let (predicate, asked) = watched(&alive);

    std::thread::scope(|scope| {
        let client = scope.spawn(|| Qmp::connect(&sock.endpoint, &predicate, &MemoryProgress::default()).map(drop));
        asked.recv().unwrap();
        asked.recv().unwrap();
        // The listener appears after the client has tried and failed.
        let (server, _) = serve(sock.listen(), 1, |_, _| {});
        client.join().unwrap().unwrap();
        server.join().unwrap();
    });
}

#[skuld::test]
fn a_machine_that_is_not_alive_fails_before_a_lock_file_is_touched() {
    let sock = sock();
    let error = Qmp::connect(&sock.endpoint, &|| false, &MemoryProgress::default())
        .err()
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(!sock.endpoint.lock_path().exists());
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
        let client = scope.spawn(|| Qmp::connect(&sock.endpoint, &|| true, &*progress).map(drop));
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
fn system_reset_is_not_done_on_the_reply_alone() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.command().unwrap();
        session.reply(json!({}));
        // No RESET event, and the connection ends.
    });

    let error = sock.connect().unwrap().system_reset().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("RESET"), "{error}");
    server.join().unwrap();
}

#[skuld::test]
fn system_reset_of_a_no_reboot_qemu_is_an_error_naming_no_reboot() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.command().unwrap();
        session.reply(json!({}));
        session.event("SHUTDOWN", Some(json!({ "guest": true, "reason": "guest-reset" })));
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
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.answer(json!({}));
        session.event("POWERDOWN", None);
        session.event("SHUTDOWN", Some(json!({ "guest": true, "reason": "guest-shutdown" })));
    });

    let mut qmp = sock.connect().unwrap();
    qmp.system_powerdown().unwrap();
    assert_eq!(qmp.wait_for_end().unwrap(), VmEnd::Exited);

    server.join().unwrap();
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
fn a_stop_event_in_a_normal_state_does_not_end_the_wait() {
    let sock = sock();
    let (server, _) = serve(sock.listen(), 1, |session, _| {
        session.event("STOP", None);
        session.answer(json!({ "status": "shutdown", "running": false }));
    });

    let mut qmp = sock.connect().unwrap();
    assert_eq!(qmp.wait_for_end().unwrap(), VmEnd::Exited);

    server.join().unwrap();
}
