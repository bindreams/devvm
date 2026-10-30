use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use russh::keys::{HashAlg, PublicKey};

use crate::ErrorKind;
use crate::access::{BootBound, ExecEnd, SshSession, VmExit};
use crate::contain::VmProcess;
use crate::contain::contain_tests::Sleeper;
use crate::exit::{ExitStatus, ExitWidth, NullProgress};
use crate::test_support::fixtures::{ChannelProgress, unicode_home};
use crate::test_support::sshd::{FAST_KEEPALIVE as FAST, Rig};

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

// Exit mapping ========================================================================================================

#[skuld::test]
fn echo_round_trips_through_exec_collect(#[fixture(unicode_home)] home: &Path) {
    let (status, stdout, stderr) = finished(&Rig::new(home).connect(), "echo hello world");
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
    let vm = VmExit::when(move || wait.recv().is_ok());
    let session = rig.connect_with(vm, None).unwrap();
    std::thread::scope(|scope| {
        let running = scope.spawn(|| session.exec_collect("hold"));
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
        sleeper.finish();
        let error = running.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
        assert!(error.message().contains("VM process exited"), "{error}");
    });
}

// Connecting ==========================================================================================================

#[skuld::test]
fn connect_retries_while_refused_and_stops_when_the_vm_exits(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let mut target = rig.target();
    target.addr = refused_addr();
    let (progress, messages) = ChannelProgress::new();
    let (fire, wait) = mpsc::channel::<()>();
    let vm = VmExit::when(move || wait.recv().is_ok());
    let error = std::thread::scope(|scope| {
        let connecting = scope.spawn(|| {
            SshSession::connect(
                &target,
                FAST,
                vm,
                Some(BootBound::start(Duration::from_secs(3600))),
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
fn host_key_mismatch_is_retried_while_the_vm_lives_then_reported_with_both_fingerprints_at_the_boot_bound(
    #[fixture(unicode_home)] home: &Path,
) {
    // A wrong key for good: retried until the bound, then `host-key` naming both fingerprints.
    let rig = Rig::with(home, |options| options.wrong_host_key_connections = usize::MAX);
    let error = rig
        .connect_with(VmExit::never(), Some(BootBound::start(Duration::from_millis(500))))
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
    assert!(rig.sshd.connections() > 1, "the mismatch was retried");

    // A server that fixes its key after three connections: the session comes up.
    let rig = Rig::with(home, |options| options.wrong_host_key_connections = 3);
    let session = rig
        .connect_with(VmExit::never(), Some(BootBound::start(Duration::from_secs(3600))))
        .expect("the key is right from the fourth connection on");
    assert_eq!(finished(&session, "exit 0").0, ExitStatus::Code(0));
    assert_eq!(rig.sshd.connections(), 4);
}

#[skuld::test]
fn a_host_key_mismatch_is_final_without_a_boot_bound(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::with(home, |options| options.wrong_host_key_connections = usize::MAX);
    let error = rig
        .connect_with(VmExit::never(), None)
        .err()
        .expect("the connect fails");
    assert_eq!(error.kind(), ErrorKind::HostKey, "{error}");
    assert_eq!(rig.sshd.connections(), 1);
}

#[skuld::test]
fn a_rejected_client_key_is_retried_while_booting_and_final_otherwise(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::with(home, |options| options.rejected_key_connections = 3);
    let session = rig
        .connect_with(VmExit::never(), Some(BootBound::start(Duration::from_secs(3600))))
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

/// The boot bound of the next two tests: long enough that connecting always finishes first.
const BOOT_BOUND: Duration = Duration::from_secs(2);

#[skuld::test]
fn a_booting_session_ends_a_hung_exec_at_the_boot_bound(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let session = rig
        .connect_with(VmExit::never(), Some(BootBound::start(BOOT_BOUND)))
        .expect("connects within the bound");
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
    let bound = BootBound::start(BOOT_BOUND);
    let (fire, wait) = mpsc::channel::<()>();
    let session = rig
        .connect_with(VmExit::when(move || wait.recv().is_ok()), Some(bound))
        .expect("connects within the bound");
    session.finish_boot();
    std::thread::scope(|scope| {
        let running = scope.spawn(|| session.exec_collect("hold"));
        // Let the bound pass: a clock fact, not a bet on the other thread. A bound that was still armed would have
        // ended the command by now; the VM exit is what ends it.
        std::thread::sleep(BOOT_BOUND);
        fire.send(()).unwrap();
        let error = running.join().unwrap().unwrap_err();
        assert!(error.message().contains("VM process exited"), "{error}");
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
        ExecEnd::HostOutputClosed(e) => assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe),
        ExecEnd::Finished(status) => panic!("a command that never ends finished: {status:?}"),
    }
    // The server saw the channel close, so its `yes` loses its pipe.
    rig.sshd.wait_for_channel_close();
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
