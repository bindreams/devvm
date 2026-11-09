//! `QemuMachine` against a fake QMP server and a fake VM process (a real process, as the stand-in for QEMU): the paths
//! where QEMU exits, or the connection fails, at the wrong moment. Windows only, like every test that freezes a process.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::QemuMachine;
use crate::Arch;
use crate::ErrorKind;
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
        let vm = self.handle();
        let watch = host::current().watch_process(vm.id()).unwrap();
        self.attached_through(vm, watch)
    }

    /// A machine found with `attach` whose process is watched through `watch`.
    fn attached_through(&self, vm: VmProcess, watch: Arc<dyn ProcessWatch>) -> QemuMachine {
        let host = host::current();
        QemuMachine::attached(
            vm,
            watch,
            self.record.clone(),
            self.sock.claim.clone(),
            host,
            Arc::new(MemoryProgress::default()),
        )
        .unwrap()
    }

    /// A machine this process launched and resumed, whose log is `log`.
    fn launched(&self, log: PathBuf, host: &'static dyn host::Host) -> QemuMachine {
        let vm = self.handle();
        let id = vm.id();
        let mut machine = QemuMachine::launched(
            vm,
            host.watch_process(id).unwrap(),
            self.record.clone(),
            self.sock.claim.clone(),
            Vec::new(),
            log,
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
    // The request arrives, QEMU exits without a reply.
    shutdown_against(&world(home), |session| {
        answer_running(session);
        session.command().expect("system_powerdown");
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

#[skuld::test]
fn a_connection_error_that_is_not_a_close_is_reported_while_qemu_lives(#[fixture(unicode_home)] home: &Path) {
    let world = world(home);
    let (server, _) = serve(world.sock.listen(), 1, |session, _| {
        session.answer(json!({ "status": "running", "running": true }));
        session.command().expect("system_powerdown");
        session.send_raw(b"\xff\xfe\n");
        // QEMU is alive: the connection stays up until the client lets go.
        while session.command().is_some() {}
    });
    let mut machine = world.attached();

    let error = machine.shutdown().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(world.qemu.lock().unwrap().is_alive().unwrap(), "the VM was waited for");
    assert!(
        host::symlink_metadata_if_present(&world.marker).unwrap().is_some(),
        "leftovers of a live machine were removed"
    );
    drop(machine);
    server.join().unwrap();
    world.end_qemu();
}

/// The server closes the session after `script` with QEMU still alive; `shutdown` returns only once QEMU is gone.
fn shutdown_waits_for_the_exit_after(home: &Path, script: impl Fn(&mut Session) + Send + 'static) {
    use std::sync::atomic::{AtomicBool, Ordering};

    let world = world(home);
    let (server, _) = serve(world.sock.listen(), 1, move |session, _| script(session));
    let mut machine = world.attached();
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
    shutdown_waits_for_the_exit_after(home, |session| {
        answer_running(session);
        session.answer(json!({}));
    });
}

/// Shutdown against a server that closes the session after `script` with QEMU still alive: an error, with the leftovers
/// of the live machine kept, and no wait for a process that nothing was asked to end.
fn shutdown_fails_when_the_session_is_lost_while_qemu_lives(
    home: &Path,
    script: impl Fn(&mut Session) + Send + 'static,
) {
    let world = world(home);
    let (server, _) = serve(world.sock.listen(), 1, move |session, _| script(session));
    let mut machine = world.attached();

    let error = machine.shutdown().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
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
fn a_control_socket_lost_before_the_request_while_qemu_lives_is_an_error_not_a_wait(
    #[fixture(unicode_home)] home: &Path,
) {
    // The server closes the session after the handshake, before `query-status` is answered.
    shutdown_fails_when_the_session_is_lost_while_qemu_lives(home, |_| {});
}

#[skuld::test]
fn a_reply_that_never_comes_after_the_request_was_written_waits_for_the_exit(#[fixture(unicode_home)] home: &Path) {
    // The server takes `system_powerdown` and closes the session without a reply: the request was delivered, so QEMU
    // may be going away, and the exit is what `shutdown` waits for.
    shutdown_waits_for_the_exit_after(home, |session| {
        answer_running(session);
        session.command().expect("system_powerdown");
    });
}

#[skuld::test]
fn a_request_that_cannot_be_written_while_qemu_lives_is_an_error_not_a_wait(#[fixture(unicode_home)] home: &Path) {
    // Nothing was sent, so nothing asked QEMU to end and waiting for it would wait for ever.
    let world = world(home);
    let (server, _) = serve(world.sock.listen(), 1, |session, _| {
        answer_running(session);
        // QEMU is alive: the connection stays up until the client lets go.
        while session.command().is_some() {}
    });
    let vm = world.handle();
    let watch = Refusing::new(host::current().watch_process(vm.id()).unwrap());
    watch.fail_writes_containing("system_powerdown", std::io::ErrorKind::BrokenPipe);
    let mut machine = world.attached_through(vm, watch.watch());

    let error = machine.shutdown().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
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
            session.command().expect("query-status");
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
    let machine = world.launched(log, host::current());

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
