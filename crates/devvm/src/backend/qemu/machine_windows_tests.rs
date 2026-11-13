//! `QemuMachine` against a fake QMP server and a fake VM process (a real process, as the stand-in for QEMU): the paths
//! where QEMU exits, or the connection fails, at the wrong moment. Windows only, like every test that freezes a process.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::QemuMachine;
use crate::Arch;
use crate::ErrorKind;
use crate::backend::fake::ask_to_crash;
use crate::backend::qemu::qmp::qmp_tests::{Log, Server, Session, serve};
use crate::backend::{Machine, MachineRecord};
use crate::contain::{Lifetime, VmProcess};
use crate::host::{self, ProcessWatch};
use crate::test_support::fake_vm;
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::qemu::Sock;
use crate::test_support::refusing::Refusing;

/// A fake VM, a control socket for it and the record that names both.
struct World {
    dir: PathBuf,
    sock: Sock,
    record: MachineRecord,
    /// The test's own handle on the VM process, to end it from a server script or at the end of the test.
    qemu: Arc<Mutex<VmProcess>>,
    /// A leftover file besides the socket's lock file.
    marker: PathBuf,
}

fn world(dir: &Path) -> World {
    let mut vm = VmProcess::spawn_frozen(
        host::current(),
        fake_vm::command(dir),
        Lifetime::DiesWithOwner,
        &fake_vm::log(dir),
    )
    .unwrap();
    vm.resume().unwrap();
    let sock = Sock::new();
    let marker = dir.join("leftover");
    std::fs::write(&marker, b"x").unwrap();
    let record = MachineRecord {
        process: vm.record().unwrap(),
        qmp: sock.endpoint.path().display().to_string(),
        ssh: Vec::new(),
        serial_log: dir.join("serial.log"),
        leftovers: vec![marker.clone(), sock.endpoint.lock_path(), sock.endpoint.hold_path()],
        claim: Some(sock.claim.clone()),
        no_reboot: false,
    };
    World {
        dir: dir.to_path_buf(),
        sock,
        record,
        qemu: Arc::new(Mutex::new(vm)),
        marker,
    }
}

impl World {
    fn handle(&self) -> VmProcess {
        VmProcess::attach(&self.record.process)
            .unwrap()
            .expect("the fake VM runs")
    }

    /// A machine found with `attach`, as another process sees one.
    fn attached(&self) -> QemuMachine {
        self.attached_watched(self.watch())
    }

    /// The host's watch on the VM process.
    fn watch(&self) -> Arc<dyn ProcessWatch> {
        host::current().watch_process(self.handle().id()).unwrap()
    }

    /// `attached`, with the process watch `watch`.
    fn attached_watched(&self, watch: Arc<dyn ProcessWatch>) -> QemuMachine {
        let host = host::current();
        QemuMachine::attached(
            self.handle(),
            watch,
            self.record.clone(),
            self.sock.claim.clone(),
            host,
            Arc::new(MemoryProgress::default()),
        )
        .unwrap()
    }

    /// A machine this process launched and resumed, whose log is `log` and whose output begins at `log_from`.
    fn launched(&self, log: PathBuf, log_from: u64, host: &'static dyn host::Host) -> QemuMachine {
        let vm = self.handle();
        let id = vm.id();
        let mut machine = QemuMachine::launched(
            vm,
            host.watch_process(id).unwrap(),
            self.record.clone(),
            self.sock.claim.clone(),
            Vec::new(),
            log,
            log_from,
            Arch::Amd64,
            host,
            Arc::new(MemoryProgress::default()),
        );
        machine.resumed = true;
        machine
    }

    /// Ends the VM process, as QEMU exiting on its own does; returns after it has exited.
    fn end_qemu(&self) {
        self.qemu.lock().unwrap().kill().unwrap();
    }

    fn lock_file(&self) -> PathBuf {
        self.sock.endpoint.lock_path()
    }
}

/// Serves one client: runs `script`, ends the VM process, and closes the connection.
fn qemu_ends_after(world: &World, script: impl Fn(&mut Session) + Send + 'static) -> (Server, Log) {
    let qemu = world.qemu.clone();
    serve(world.sock.listen(), 1, move |session: &mut Session, _| {
        script(session);
        qemu.lock().unwrap().kill().unwrap();
    })
}

/// A running VM's answer to `query-status`.
fn answer_running(session: &mut Session) {
    session.answer(json!({ "status": "running", "running": true }));
}

/// Shuts `world`'s machine down against a server that runs `script` and then ends QEMU; the leftovers are removed.
fn shutdown_against(world: &World, script: impl Fn(&mut Session) + Send + 'static) {
    let (server, _) = qemu_ends_after(world, script);
    let mut machine = world.attached();

    machine.shutdown().unwrap();

    server.join().unwrap();
    assert!(
        host::symlink_metadata_if_present(&world.marker).unwrap().is_none(),
        "the leftover was not removed"
    );
    assert!(
        host::symlink_metadata_if_present(&world.lock_file()).unwrap().is_none(),
        "the lock file was not removed"
    );
}

#[skuld::test]
fn qemu_gone_before_the_first_command_is_a_shutdown(#[fixture(unicode_home)] home: &Path) {
    shutdown_against(&world(home), |_| {});
}

#[skuld::test]
fn qemu_that_exits_after_the_status_query_is_a_shutdown(#[fixture(unicode_home)] home: &Path) {
    shutdown_against(&world(home), answer_running);
}

#[skuld::test]
fn qemu_that_exits_after_the_ack_is_a_shutdown(#[fixture(unicode_home)] home: &Path) {
    shutdown_against(&world(home), |session| {
        answer_running(session);
        session.answer(json!({}));
    });
}

#[skuld::test]
fn a_failed_request_with_qemu_gone_is_a_shutdown(#[fixture(unicode_home)] home: &Path) {
    // The request arrives, QEMU exits without a reply: the lost session finds QEMU gone.
    shutdown_against(&world(home), |session| {
        answer_running(session);
        session.expect_command("system_powerdown");
    });
}

#[skuld::test]
fn shutdown_of_a_machine_that_is_gone_removes_its_leftovers(#[fixture(unicode_home)] home: &Path) {
    let world = world(home);
    let mut machine = world.attached();
    world.end_qemu();

    machine.shutdown().unwrap();

    assert!(
        host::symlink_metadata_if_present(&world.marker).unwrap().is_none(),
        "the leftover was not removed"
    );
}

/// Runs `script` against a machine whose QEMU stays alive, with the link's `Refusing` to make a loss with. A lost
/// session means QEMU is gone or going, so `shutdown` returns only once QEMU is gone: this ends the VM after the script
/// is done and checks that `shutdown` did not return before.
///
/// The loss is made on the client's link, never by the server closing: Windows can lose the end of the stream, so the
/// client would wait for it for ever. The script makes the link fail, then sends an event so that a read that is
/// blocked already returns.
fn shutdown_waits_for_the_exit_after(home: &Path, script: impl Fn(&mut Session, &Refusing) + Send + 'static) {
    use std::sync::atomic::{AtomicBool, Ordering};

    let world = world(home);
    let watch = Refusing::new(world.watch());
    let seen_by_script = watch.clone();
    let (server, _) = serve(world.sock.listen(), 1, move |session, _| {
        script(session, &seen_by_script)
    });
    let mut machine = world.attached_watched(watch.watch());
    let exited = AtomicBool::new(false);

    let (result, exited_before_it_returned) = std::thread::scope(|scope| {
        let shutdown = scope.spawn(|| {
            let result = machine.shutdown();
            (result, exited.load(Ordering::SeqCst))
        });
        server.wait().unwrap();
        exited.store(true, Ordering::SeqCst);
        world.end_qemu();
        shutdown.join().unwrap()
    });

    result.unwrap();
    assert!(exited_before_it_returned, "shutdown returned while QEMU was alive");
}

#[skuld::test]
fn shutdown_waits_for_the_exit_after_the_control_socket_closes_after_the_ack_while_qemu_lives(
    #[fixture(unicode_home)] home: &Path,
) {
    shutdown_waits_for_the_exit_after(home, |session, _| {
        answer_running(session);
        session.answer(json!({}));
    });
}

/// The client's reads fail as a reset connection does, after the server has seen the command `after`.
fn lost_after(session: &mut Session, watch: &Refusing, after: &str) {
    session.expect_command(after);
    watch.fail_reads_with(std::io::ErrorKind::ConnectionReset);
    session.event("RESUME", None);
}

#[skuld::test]
fn shutdown_waits_for_the_exit_after_the_control_socket_is_lost_before_the_status_reply(
    #[fixture(unicode_home)] home: &Path,
) {
    shutdown_waits_for_the_exit_after(home, |session, watch| lost_after(session, watch, "query-status"));
}

#[skuld::test]
fn shutdown_waits_for_the_exit_after_the_control_socket_is_lost_before_the_ack(#[fixture(unicode_home)] home: &Path) {
    shutdown_waits_for_the_exit_after(home, |session, watch| {
        answer_running(session);
        lost_after(session, watch, "system_powerdown");
    });
}

#[skuld::test]
fn shutdown_waits_for_the_exit_after_a_request_cannot_be_written_because_the_socket_closed(
    #[fixture(unicode_home)] home: &Path,
) {
    shutdown_waits_for_the_exit_after(home, |session, watch| {
        // The client's `query-status` is in; its next write, the request, fails as a closed socket's does.
        session.expect_command("query-status");
        watch.fail_writes_with(std::io::ErrorKind::BrokenPipe);
        session.reply(json!({ "status": "running", "running": true }));
    });
}

/// Shutdown of a machine whose QEMU stays alive, against a server that runs `script` once with the link's `Refusing`:
/// an error of `kind`, with the leftovers of the live machine kept, and no wait for a process that nothing was asked
/// to end.
fn shutdown_fails_while_qemu_lives(
    home: &Path,
    kind: ErrorKind,
    script: impl Fn(&mut Session, &Refusing) + Send + 'static,
) {
    let world = world(home);
    let watch = Refusing::new(world.watch());
    let seen_by_script = watch.clone();
    let (server, _) = serve(world.sock.listen(), 1, move |session, _| {
        script(session, &seen_by_script)
    });
    let mut machine = world.attached_watched(watch.watch());

    let error = machine.shutdown().unwrap_err();

    assert_eq!(error.kind(), kind, "{error}");
    assert!(world.qemu.lock().unwrap().is_alive().unwrap(), "the VM was waited for");
    assert!(
        host::symlink_metadata_if_present(&world.marker).unwrap().is_some(),
        "leftovers of a live machine were removed"
    );
    drop(machine);
    server.join().unwrap();
    world.end_qemu();
}

#[skuld::test]
fn a_request_that_cannot_be_written_for_a_reason_that_is_not_a_close_is_an_error_not_a_wait(
    #[fixture(unicode_home)] home: &Path,
) {
    shutdown_fails_while_qemu_lives(home, ErrorKind::Io, |session, watch| {
        session.expect_command("query-status");
        watch.fail_writes_with(std::io::ErrorKind::PermissionDenied);
        session.reply(json!({ "status": "running", "running": true }));
    });
}

#[skuld::test]
fn a_connection_error_that_is_not_a_close_is_reported_while_qemu_lives(#[fixture(unicode_home)] home: &Path) {
    shutdown_fails_while_qemu_lives(home, ErrorKind::Io, |session, _| {
        answer_running(session);
        session.expect_command("system_powerdown");
        session.send_raw(b"\xff\xfe\n");
        // QEMU is alive: the connection stays up until the test releases the server.
        while session.command().is_some() {}
    });
}

#[skuld::test]
fn reset_of_a_machine_that_exits_during_the_state_check_quotes_the_log(#[fixture(unicode_home)] home: &Path) {
    let world = world(home);
    std::fs::write(world.dir.join("qemu.log"), "qemu: out of memory").unwrap();
    let qemu = world.qemu.clone();
    let clients = std::sync::atomic::AtomicUsize::new(0);
    let (server, _) = serve(world.sock.listen(), 2, move |session: &mut Session, _| {
        if clients.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            // An attached machine asks for its architecture first; aarch64 is one every host can reset.
            session.answer(json!({ "arch": "aarch64" }));
        } else {
            // QEMU dies on the state check.
            session.expect_command("query-status");
            qemu.lock().unwrap().kill().unwrap();
        }
    });
    let mut machine = world.attached();

    let error = machine.reset().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("out of memory"), "{error}");
    drop(machine);
    server.join().unwrap();
}

#[skuld::test]
fn the_log_a_launched_machine_quotes_is_the_one_it_was_given(#[fixture(unicode_home)] home: &Path) {
    let world = world(home);
    let elsewhere = home.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let log = elsewhere.join("qemu.log");
    std::fs::write(&log, "qemu: could not open the disk").unwrap();
    let machine = world.launched(log, 0, host::current());

    let error = machine.exited("gone");

    assert!(error.message().contains("could not open the disk"), "{error}");
    world.end_qemu();
}

#[skuld::test]
fn the_log_an_attached_machine_quotes_is_beside_the_serial_log(#[fixture(unicode_home)] home: &Path) {
    let world = world(home);
    std::fs::write(world.dir.join("qemu.log"), "qemu: out of memory").unwrap();
    let machine = world.attached();

    let error = machine.exited("gone");

    assert!(error.message().contains("out of memory"), "{error}");
    world.end_qemu();
}

/// Appends `text` to the file at `path`, as a launch of QEMU appends to its log.
fn append(path: &Path, text: &str) {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

#[skuld::test]
fn the_output_a_launched_machine_quotes_is_only_what_its_own_launch_printed(#[fixture(unicode_home)] home: &Path) {
    let world = world(home);
    let log = home.join("qemu.log");
    append(&log, "qemu: an earlier launch failed\n");
    let from = std::fs::metadata(&log).unwrap().len();
    append(&log, "qemu: this launch ran out of memory\n");
    let machine = world.launched(log, from, host::current());

    let error = machine.exited("gone");

    assert!(error.message().contains("this launch ran out of memory"), "{error}");
    assert!(!error.message().contains("an earlier launch"), "{error}");
    world.end_qemu();
}

#[skuld::test]
fn a_launched_machine_that_exited_gives_its_status_and_the_output_of_its_own_launch(
    #[fixture(unicode_home)] home: &Path,
) {
    let World { qemu, record, sock, .. } = world(home);
    let vm = Arc::try_unwrap(qemu)
        .ok()
        .expect("nothing else holds the VM")
        .into_inner()
        .unwrap();
    let port = fake_vm::await_port(&vm, home);
    let log = home.join("qemu.log");
    append(&log, "qemu: an earlier launch failed\n");
    let from = std::fs::metadata(&log).unwrap().len();
    append(&log, "qemu: this launch ran out of memory\n");
    let host = host::current();
    let watch = host.watch_process(vm.id()).unwrap();
    let mut machine = QemuMachine::launched(
        vm,
        watch,
        record,
        sock.claim.clone(),
        Vec::new(),
        log,
        from,
        Arch::Amd64,
        host,
        Arc::new(MemoryProgress::default()),
    );
    machine.resumed = true;
    assert!(machine.try_wait().unwrap().is_none(), "the VM runs");

    ask_to_crash(std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port))).unwrap();
    machine.vm.wait().unwrap();

    let exit = machine.try_wait().unwrap().expect("the VM has exited");
    assert_eq!(exit.status.code(), Some(1), "{}", exit.status);
    assert!(exit.output.contains("this launch ran out of memory"), "{}", exit.output);
    assert!(!exit.output.contains("an earlier launch"), "{}", exit.output);
    machine.vm.kill().unwrap();
}
