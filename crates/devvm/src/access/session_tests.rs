use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use russh::keys::{HashAlg, PublicKey};

use crate::ErrorKind;
use crate::access::session::{BootGate, ClientHandler, Transport};
use crate::access::{BootBound, ExecEnd, Keepalive, SshSession, VmExit};
use crate::contain::VmProcess;
use crate::contain::contain_tests::Sleeper;
use crate::exit::{ExitStatus, ExitWidth, NullProgress};
use crate::test_support::fixtures::{ChannelProgress, unicode_home};
use crate::test_support::sshd::{FAST_KEEPALIVE as FAST, Rig, TestSshd};

fn finished(session: &SshSession, command: &str) -> (ExitStatus, Vec<u8>, Vec<u8>) {
    let output = session.exec_collect(command).unwrap();
    (output.status, output.stdout, output.stderr)
}

fn fingerprint(public: &str) -> String {
    PublicKey::from_openssh(public)
        .unwrap()
        .fingerprint(HashAlg::Sha256)
        .to_string()
}

/// An address nothing listens on.
fn refused_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

// Streams =============================================================================================================

#[skuld::test]
fn output_on_an_extended_data_stream_other_than_stderr_is_an_error_naming_it(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let session = rig.connect();
    let error = session.exec_collect("extended-data 2").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("extended data stream 2"), "{error}");
    // Stream 1 is stderr, as ever.
    let output = session.exec_collect("extended-data 1").unwrap();
    assert_eq!(
        output.stderr,
        b"on a stream of its own
"
    );
}

#[skuld::test]
fn output_lost_after_the_exit_status_is_reported_with_the_status(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let session = rig.connect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    match session.exec("status-then-extended-data 2", &mut out, &mut err).unwrap() {
        ExecEnd::FinishedWithLoss { status, lost } => {
            assert_eq!(
                status,
                ExitStatus::Code(7),
                "the status is not hidden by what came after"
            );
            assert!(lost.message().contains("extended data stream 2"), "{lost}");
        }
        other => panic!("{other:?}"),
    }
    // Collected into memory, the status and the output that came before the loss are kept, and the loss is carried.
    let output = session.exec_collect("status-then-extended-data 2").unwrap();
    assert_eq!(output.status, ExitStatus::Code(7));
    assert_eq!(
        output.stdout,
        b"before the status
"
    );
    let loss = output.loss.expect("the loss is carried with the output");
    assert!(loss.message().contains("extended data stream 2"), "{loss}");
}

// Exit mapping ========================================================================================================

#[skuld::test]
fn echo_round_trips_through_exec_collect(#[fixture(unicode_home)] home: &Path) {
    let output = Rig::new(home).connect().exec_collect("echo hello world").unwrap();
    assert!(output.loss.is_none(), "{:?}", output.loss);
    let (status, stdout, stderr) = (output.status, output.stdout, output.stderr);
    assert_eq!(status, ExitStatus::Code(0));
    assert_eq!(stdout, b"hello world\n");
    assert!(stderr.is_empty());
}

#[skuld::test]
fn exit_7_125_255_pass_through_unchanged(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let session = rig.connect();
    for code in [7, 125, 255] {
        let (status, ..) = finished(&session, &format!("exit {code}"));
        assert_eq!(status, ExitStatus::Code(code));
    }
    // A guest's own `devvm-error:` line and 125 are the guest's, not devvm's.
    let (status, stdout, _) = finished(&session, "say-error-and-exit-125");
    assert_eq!(status, ExitStatus::Code(125));
    assert_eq!(stdout, b"devvm-error: x: y\n");
}

#[skuld::test]
fn signal_death_segv_and_kill_map_to_signal(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let session = rig.connect();
    let (segv, ..) = finished(&session, "signal SEGV core");
    assert_eq!(
        segv,
        ExitStatus::Signal {
            name: "SEGV".to_owned(),
            core_dumped: true
        }
    );
    let (kill, ..) = finished(&session, "signal KILL");
    assert_eq!(
        kill,
        ExitStatus::Signal {
            name: "KILL".to_owned(),
            core_dumped: false
        }
    );
    assert_eq!(ExitWidth::LowByte.exit_code(&segv), 139);
    assert_eq!(ExitWidth::LowByte.exit_code(&kill), 137);
}

#[skuld::test]
fn closed_without_status_is_no_exit_status(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let session = rig.connect();
    let error = session.exec_collect("close-no-status").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NoExitStatus);
    // The connection is fine afterwards.
    assert_eq!(finished(&session, "exit 0").0, ExitStatus::Code(0));
}

#[skuld::test]
fn mid_command_drop_is_connection_lost(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let session = rig.connect();
    let error = session.exec_collect("drop-connection").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("drop-connection"), "{error}");
    assert!(
        error.message().contains(&rig.serial_log.display().to_string()),
        "{error}"
    );
}

#[skuld::test]
fn a_frozen_peer_is_connection_lost_through_keepalive(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let session = rig.connect();
    let error = session.exec_collect("freeze").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("keepalive"), "{error}");
}

#[skuld::test]
fn all_256_byte_values_arrive_intact_on_both_streams(#[fixture(unicode_home)] home: &Path) {
    const MIB: usize = 4;
    let rig = Rig::new(home);
    let session = rig.connect();
    let (status, stdout, stderr) = finished(&session, &format!("pattern {MIB}"));
    assert_eq!(status, ExitStatus::Code(0));
    assert_eq!(stdout.len(), MIB << 20);
    assert_eq!(stderr.len(), MIB << 20);
    assert!(
        stdout.iter().enumerate().all(|(i, b)| *b == (i % 256) as u8),
        "stdout differs"
    );
    assert!(
        stderr.iter().enumerate().all(|(i, b)| *b == 255 - (i % 256) as u8),
        "stderr differs"
    );
}

#[skuld::test]
fn a_vm_exit_ends_a_running_exec_as_connection_lost(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let (fire, wait) = mpsc::channel::<()>();
    let vm = VmExit::when(move || wait.recv().map_err(|e| e.to_string()));
    let session = rig.connect_with(vm, None).unwrap();
    std::thread::scope(|scope| {
        let running = scope.spawn(|| session.exec_collect("hold"));
        // The command is in flight on the server, so the exit ends a wait that is running, not one about to start.
        rig.sshd.wait_for_commands(1);
        fire.send(()).unwrap();
        let error = running.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
        assert!(error.message().contains("VM process exited"), "{error}");
        assert!(
            error.message().contains(&rig.serial_log.display().to_string()),
            "{error}"
        );
    });
}

/// A real child process is the `VmExit` source: killed, it ends the command.
#[skuld::test]
fn a_process_exit_ends_a_running_exec_as_connection_lost(#[fixture(unicode_home)] home: &Path) {
    let sleeper = Sleeper::spawn();
    let process = VmProcess::attach(&sleeper.record())
        .unwrap()
        .expect("the sleeper is alive");
    let rig = Rig::new(home);
    let session = rig.connect_with(VmExit::watch(process), None).unwrap();
    std::thread::scope(|scope| {
        let running = scope.spawn(|| session.exec_collect("hold"));
        rig.sshd.wait_for_commands(1);
        sleeper.finish();
        let error = running.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
        assert!(error.message().contains("VM process exited"), "{error}");
    });
}

#[skuld::test]
fn a_vm_watcher_that_cannot_wait_ends_the_wait_with_its_reason(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let (fire, wait) = mpsc::channel::<()>();
    let vm = VmExit::when(move || {
        let _ = wait.recv();
        Err("the handle is gone".to_owned())
    });
    let session = rig.connect_with(vm, None).unwrap();
    std::thread::scope(|scope| {
        let running = scope.spawn(|| session.exec_collect("hold"));
        rig.sshd.wait_for_commands(1);
        fire.send(()).unwrap();
        let error = running.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
        assert!(error.message().contains("the handle is gone"), "{error}");
        assert!(!error.message().contains("VM process exited"), "{error}");
    });
}

#[skuld::test]
fn a_keepalive_that_tolerates_no_missed_probe_is_refused(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let ka = Keepalive {
        interval: Duration::from_millis(100),
        max_missed: 0,
    };
    let error = SshSession::connect(&rig.target(), ka, VmExit::never(), None, &NullProgress)
        .err()
        .expect("the connect fails");
    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(error.message().contains("max_missed"), "{error}");
    assert_eq!(rig.sshd.connections(), 0, "refused before any connection");
}

#[skuld::test]
fn the_disconnect_hook_keeps_the_reason_that_ended_the_connection_first(#[fixture(unicode_home)] home: &Path) {
    use russh::client::{DisconnectReason, Handler};

    let rig = Rig::new(home);
    let pinned = PublicKey::from_openssh(&rig.target().host_public).unwrap();
    let transport = Transport::new();
    transport.end("the stream ended".to_owned());
    let mut handler = ClientHandler {
        pinned,
        transport: transport.clone(),
    };
    crate::access::runtime::run("running the disconnect hook", async move {
        handler
            .disconnected(DisconnectReason::Error(russh::Error::KeepaliveTimeout))
            .await
    })
    .unwrap()
    .unwrap_err();
    assert_eq!(transport.ended.borrow().as_deref(), Some("the stream ended"));
}

#[skuld::test]
fn a_time_bound_that_would_overflow_or_never_fire_is_refused_naming_it() {
    let keepalive = |interval, max_missed| Keepalive { interval, max_missed }.handshake_bound().map(drop);
    let boot = |limit| BootBound::start(limit).map(drop);
    // Every configurable bound: (what is set, the result of setting it, the name the refusal must carry).
    let refused = [
        ("keepalive interval of 0", keepalive(Duration::ZERO, 3), "interval"),
        (
            "keepalive max_missed of 0",
            keepalive(Duration::from_secs(1), 0),
            "max_missed",
        ),
        (
            "keepalive interval of Duration::MAX",
            keepalive(Duration::MAX, 3),
            "handshake bound",
        ),
        (
            "keepalive max_missed of u32::MAX",
            keepalive(Duration::from_secs(1), u32::MAX),
            "handshake bound",
        ),
        ("boot limit of Duration::MAX", boot(Duration::MAX), "boot bound"),
    ];
    for (what, result, name) in refused {
        let error = result.err().unwrap_or_else(|| panic!("{what} is accepted"));
        assert_eq!(error.kind(), ErrorKind::Internal, "{what}: {error}");
        assert!(error.message().contains(name), "{what}: {error}");
    }
    // A boot limit of 0 is not degenerate: the deadline has passed and the wait ends at once, naming the limit.
    assert!(boot(Duration::ZERO).is_ok());
}

// Connecting ==========================================================================================================

#[skuld::test]
fn connect_retries_while_refused_and_stops_when_the_vm_exits(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let mut target = rig.target();
    target.addr = refused_addr();
    let (progress, messages) = ChannelProgress::new();
    let (fire, wait) = mpsc::channel::<()>();
    let vm = VmExit::when(move || wait.recv().map_err(|e| e.to_string()));
    let error = std::thread::scope(|scope| {
        let connecting = scope.spawn(|| {
            SshSession::connect(
                &target,
                FAST,
                vm,
                Some(BootBound::start(Duration::from_secs(3600)).unwrap()),
                &*progress,
            )
        });
        // A refusal is retried: the first one is reported as progress, and the connect is still going.
        let message = messages.recv().unwrap();
        assert!(message.contains("waiting for SSH"), "{message}");
        fire.send(()).unwrap();
        connecting.join().unwrap().err().expect("the connect fails")
    });
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("VM process exited"), "{error}");
    assert!(error.message().contains(&target.addr.to_string()), "{error}");
}

#[skuld::test]
fn a_connect_without_a_boot_bound_to_a_peer_that_accepts_and_never_speaks_ends_in_connection_lost_naming_the_address_and_the_bound(
    #[fixture(unicode_home)] home: &Path,
) {
    let rig = Rig::with(home, |options| options.silent = true);
    let error = rig
        .connect_with(VmExit::never(), None)
        .err()
        .expect("the connect fails");
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains(&rig.sshd.addr().to_string()), "{error}");
    assert!(error.message().contains("handshake bound"), "{error}");
    assert!(
        error.message().contains(&rig.serial_log.display().to_string()),
        "{error}"
    );
}

#[skuld::test]
fn a_connect_without_a_boot_bound_to_a_refused_port_is_connection_lost_at_once(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let mut target = rig.target();
    target.addr = refused_addr();
    let error = SshSession::connect(&target, FAST, VmExit::never(), None, &NullProgress)
        .err()
        .expect("the connect fails");
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains(&target.addr.to_string()), "{error}");
}

#[skuld::test]
fn a_host_key_mismatch_is_retried_while_booting(#[fixture(unicode_home)] home: &Path) {
    // A server that fixes its key after three connections: the session comes up, so the mismatch was retried.
    let rig = Rig::with(home, |options| options.wrong_host_key_connections = 3);
    let session = rig
        .connect_with(
            VmExit::never(),
            Some(BootBound::start(Duration::from_secs(3600)).unwrap()),
        )
        .expect("the key is right from the fourth connection on");
    assert_eq!(finished(&session, "exit 0").0, ExitStatus::Code(0));
    assert_eq!(rig.sshd.connections(), 4);
}

#[skuld::test]
fn a_host_key_mismatch_is_final_without_a_boot_bound_and_names_both_fingerprints(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::with(home, |options| options.wrong_host_key_connections = usize::MAX);
    let error = rig
        .connect_with(VmExit::never(), None)
        .err()
        .expect("the connect fails");
    assert_eq!(error.kind(), ErrorKind::HostKey, "{error}");
    assert!(
        error.message().contains(&fingerprint(&rig.sshd.host_public())),
        "{error}"
    );
    assert!(
        error.message().contains(&fingerprint(&rig.sshd.wrong_host_public())),
        "{error}"
    );
    assert_eq!(rig.sshd.connections(), 1);
}

#[skuld::test]
fn a_rejected_client_key_is_retried_while_booting_and_final_otherwise(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::with(home, |options| options.rejected_key_connections = 3);
    let session = rig
        .connect_with(
            VmExit::never(),
            Some(BootBound::start(Duration::from_secs(3600)).unwrap()),
        )
        .expect("the key is accepted from the fourth connection on");
    assert_eq!(finished(&session, "exit 0").0, ExitStatus::Code(0));
    assert_eq!(rig.sshd.connections(), 4);

    let rig = Rig::with(home, |options| options.rejected_key_connections = 1);
    let error = rig
        .connect_with(VmExit::never(), None)
        .err()
        .expect("the connect fails");
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("`user`"), "{error}");
    assert!(error.message().contains("refused"), "{error}");
    assert_eq!(rig.sshd.connections(), 1);
}

// The boot bound ======================================================================================================

/// Starts the boot bound of `session` now, as the engine does when it resumes the VM. The tests start it after the
/// connect, so how long the connect took cannot matter.
fn arm_boot_bound(session: &SshSession, limit: Duration) -> BootBound {
    let bound = BootBound::start(limit).unwrap();
    session.inner.boot.0.send_replace(Some(bound));
    bound
}

#[skuld::test]
fn a_booting_session_ends_a_hung_exec_at_the_boot_bound(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let one_hour = Duration::from_secs(3600);
    let session = rig
        .connect_with(VmExit::never(), Some(BootBound::start(one_hour).unwrap()))
        .expect("connects within the bound");
    // The bound a connect was given is the session's.
    assert_eq!(session.inner.boot.0.borrow().map(|bound| bound.limit), Some(one_hour));
    arm_boot_bound(&session, Duration::from_millis(200));
    let error = session.exec_collect("hold").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("boot bound"), "{error}");
    assert!(error.message().contains("hold"), "{error}");
    assert!(
        error.message().contains(&rig.serial_log.display().to_string()),
        "{error}"
    );
}

#[skuld::test]
fn a_finished_boot_is_no_longer_ended_by_the_boot_bound(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let (fire, wait) = mpsc::channel::<()>();
    let session = rig
        .connect_with(VmExit::when(move || wait.recv().map_err(|e| e.to_string())), None)
        .expect("connects");
    // A bound that has already passed: were it still armed, it would end the command at once.
    arm_boot_bound(&session, Duration::ZERO);
    session.finish_boot();
    std::thread::scope(|scope| {
        let running = scope.spawn(|| session.exec_collect("hold"));
        rig.sshd.wait_for_commands(1);
        fire.send(()).unwrap();
        let error = running.join().unwrap().unwrap_err();
        assert!(error.message().contains("VM process exited"), "{error}");
    });
}

#[skuld::test]
fn the_boot_gate_follows_changes_while_a_wait_is_in_flight() {
    // Virtual time: the deadlines below pass without anyone waiting for them.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let gate = BootGate::new(Some(BootBound::start(Duration::from_secs(10)).unwrap()));
        let mut passed = std::pin::pin!(gate.passed());
        let quiet = |seconds| tokio::time::sleep(Duration::from_secs(seconds));

        tokio::select! {
            _ = &mut passed => panic!("the bound passed early"),
            () = quiet(5) => {}
        }
        // Finishing the boot disarms the wait that is already in flight.
        gate.finish();
        tokio::select! {
            _ = &mut passed => panic!("a finished boot was ended by its bound"),
            () = quiet(60) => {}
        }
        // A bound armed later governs it again.
        gate.0
            .send_replace(Some(BootBound::start(Duration::from_secs(1)).unwrap()));
        let bound = passed.await;
        assert_eq!(bound.limit, Duration::from_secs(1));
    });
}

// Host output =========================================================================================================

struct RefusesWrites;

impl Write for RefusesWrites {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "the reader is gone",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[skuld::test]
fn a_closed_host_output_closes_the_channel_and_returns_host_output_closed(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let session = rig.connect();
    let mut stderr = Vec::new();
    let end = session.exec("write-forever", &mut RefusesWrites, &mut stderr).unwrap();
    match end {
        ExecEnd::HostOutputClosed { error, status } => {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            assert_eq!(status, None, "the command had not ended");
        }
        other => panic!("a command that never ends finished: {other:?}"),
    }
    // The server saw the channel close, so its `yes` loses its pipe.
    rig.sshd.wait_for_channel_close();
}

#[skuld::test]
fn a_closed_host_output_closes_the_channel_of_a_command_that_went_quiet(#[fixture(unicode_home)] home: &Path) {
    // `tail -f log | head -1`: the guest wrote once, the host output closed, and the guest says nothing more. Only the
    // close wakes the task that owns the channel.
    let rig = Rig::new(home);
    let session = rig.connect();
    let mut stderr = Vec::new();
    let end = session
        .exec("write-once-and-hold", &mut RefusesWrites, &mut stderr)
        .unwrap();
    assert!(matches!(end, ExecEnd::HostOutputClosed { status: None, .. }), "{end:?}");
    rig.sshd.wait_for_channel_close();
}

/// A sink that drops every connection of `sshd` when it receives output.
struct DropsConnections<'a>(&'a TestSshd);

impl Write for DropsConnections<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.drop_connections();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[skuld::test]
fn an_exit_status_that_arrived_is_the_result_even_if_the_connection_dies_before_the_close(
    #[fixture(unicode_home)] home: &Path,
) {
    // The marker follows the status on the wire, so the status has been read when the sink sees the marker.
    let rig = Rig::new(home);
    let session = rig.connect();
    let mut stderr = Vec::new();
    let end = session
        .exec("status-then-marker", &mut DropsConnections(&rig.sshd), &mut stderr)
        .unwrap();
    match end {
        ExecEnd::Finished(status) => assert_eq!(status, ExitStatus::Code(7)),
        other => panic!("{other:?}"),
    }
}

/// A sink that runs `action` when it receives output.
struct RunsOnWrite<F: Fn()>(F);

impl<F: Fn()> Write for RunsOnWrite<F> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (self.0)();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[skuld::test]
fn an_exit_status_that_arrived_is_the_result_and_the_vm_exit_after_it_is_the_loss(
    #[fixture(unicode_home)] home: &Path,
) {
    let rig = Rig::new(home);
    let (fire, wait) = mpsc::channel::<()>();
    let session = rig
        .connect_with(VmExit::when(move || wait.recv().map_err(|e| e.to_string())), None)
        .unwrap();
    let mut stderr = Vec::new();
    // The marker follows the status on the wire, so the status has been read when the sink sees the marker.
    let end = session
        .exec(
            "status-then-marker",
            &mut RunsOnWrite(|| {
                let _ = fire.send(());
            }),
            &mut stderr,
        )
        .unwrap();
    match end {
        ExecEnd::FinishedWithLoss { status, lost } => {
            assert_eq!(status, ExitStatus::Code(7));
            assert_eq!(lost.kind(), ErrorKind::ConnectionLost, "{lost}");
            assert!(lost.message().contains("VM process exited"), "{lost}");
        }
        other => panic!("{other:?}"),
    }
}

#[skuld::test]
fn an_exit_status_that_arrived_is_the_result_and_the_boot_bound_after_it_is_the_loss(
    #[fixture(unicode_home)] home: &Path,
) {
    let rig = Rig::new(home);
    let session = rig
        .connect_with(
            VmExit::never(),
            Some(BootBound::start(Duration::from_secs(3600)).unwrap()),
        )
        .unwrap();
    let mut stderr = Vec::new();
    let end = session
        .exec(
            "status-then-marker",
            &mut RunsOnWrite(|| {
                arm_boot_bound(&session, Duration::ZERO);
            }),
            &mut stderr,
        )
        .unwrap();
    match end {
        ExecEnd::FinishedWithLoss { status, lost } => {
            assert_eq!(status, ExitStatus::Code(7));
            assert_eq!(lost.kind(), ErrorKind::ConnectionLost, "{lost}");
            assert!(lost.message().contains("boot"), "{lost}");
        }
        other => panic!("{other:?}"),
    }
}

/// Counts flushes and refuses nothing.
#[derive(Default)]
struct CountsFlushes {
    flushes: usize,
}

impl Write for CountsFlushes {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

#[skuld::test]
fn exec_flushes_both_host_writers_when_the_command_ends(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let session = rig.connect();
    let (mut out, mut err) = (CountsFlushes::default(), CountsFlushes::default());
    session.exec("echo hi", &mut out, &mut err).unwrap();
    assert!(out.flushes >= 1, "stdout was flushed {} times", out.flushes);
    assert!(err.flushes >= 1, "stderr was flushed {} times", err.flushes);
}

/// A sink whose flush fails.
struct FailsToFlush;

impl Write for FailsToFlush {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "the reader is gone",
        ))
    }
}

#[skuld::test]
fn a_flush_that_fails_at_the_end_is_a_closed_host_output_that_keeps_the_status(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let session = rig.connect();
    let mut stderr = Vec::new();
    let end = session.exec("echo hi", &mut FailsToFlush, &mut stderr).unwrap();
    match end {
        ExecEnd::HostOutputClosed { error, status } => {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            assert_eq!(status, Some(ExitStatus::Code(0)), "the command's result is not lost");
        }
        other => panic!("{other:?}"),
    }
}

// The facade ==========================================================================================================

#[skuld::test]
fn blocking_api_works_inside_a_tokio_runtime(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let outer = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .unwrap();
    let target = rig.target();
    let status = outer.block_on(async move {
        tokio::spawn(async move {
            let session = SshSession::connect(&target, FAST, VmExit::never(), None, &NullProgress).unwrap();
            session.exec_collect("exit 3").unwrap().status
        })
        .await
        .unwrap()
    });
    assert_eq!(status, ExitStatus::Code(3));
}

#[skuld::test]
fn sessions_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SshSession>();
    assert_send_sync::<VmExit>();
}

// The end of the connection, whichever way it comes ===================================================================

#[skuld::test]
fn the_handshake_bound_is_the_time_the_keepalive_needs_to_give_up(#[fixture(unicode_home)] _home: &Path) {
    // The peer is declared dead once more than `max_missed` probes went unanswered: the first probe goes out after one
    // silent interval, and the last allowed miss is followed by one more interval before the verdict.
    let ka = Keepalive {
        interval: Duration::from_millis(100),
        max_missed: 2,
    };
    assert_eq!(ka.handshake_bound().unwrap(), Duration::from_millis(300));
    assert_eq!(Keepalive::STANDARD.handshake_bound().unwrap(), Duration::from_secs(20));
}

#[skuld::test]
fn a_keepalive_whose_handshake_bound_overflows_is_refused(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let too_many = Keepalive {
        interval: Duration::from_millis(100),
        max_missed: u32::MAX,
    };
    let too_long = Keepalive {
        interval: Duration::MAX,
        max_missed: 3,
    };
    for ka in [too_many, too_long] {
        assert!(ka.handshake_bound().is_err(), "{ka:?}");
        let error = SshSession::connect(&rig.target(), ka, VmExit::never(), None, &NullProgress)
            .err()
            .expect("the connect fails");
        assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
        assert!(error.message().contains("handshake bound"), "{error}");
    }
    assert_eq!(rig.sshd.connections(), 0, "refused before any connection");
}

#[skuld::test]
fn a_reset_connection_ends_a_pending_sftp_request(#[fixture(unicode_home)] home: &Path) {
    // An `RST` makes russh end its session on a path that does not tell the handler (measured: `ENOTCONN` on Linux), so
    // the end of the connection must be seen at the stream, or a request in flight waits forever.
    let rig = Rig::with(home, |options| options.hold_realpath = true);
    let session = rig.connect();
    let sftp = session.sftp().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| sender.send(sftp.home()).unwrap());
        rig.sshd.wait_for_held_request();
        rig.sshd.reset_connections();
        let error = receiver.recv().unwrap().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    });
}
