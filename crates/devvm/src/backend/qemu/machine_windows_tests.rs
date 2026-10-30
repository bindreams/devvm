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
use crate::host;
use crate::test_support::fake_vm;
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::host::{NO_GUEST_RESET, WINDOWS_11};
use crate::test_support::qemu::Sock;

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
        leftovers: vec![marker.clone(), sock.endpoint.lock_path()],
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
        QemuMachine::attached(
            self.handle(),
            self.record.clone(),
            host::current(),
            Arc::new(MemoryProgress::default()),
        )
    }

    /// A machine this process launched and resumed, whose log is `log`.
    fn launched(&self, log: PathBuf, host: &'static dyn host::Host) -> QemuMachine {
        let mut machine = QemuMachine::launched(
            self.handle(),
            self.record.clone(),
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

/// Serves one client: answers `answered` commands as a running VM would, ends the VM process, and closes the connection.
fn exits_after(world: &World, answered: usize) -> (Server, Log) {
    let qemu = world.qemu.clone();
    serve(world.sock.listen(), 1, move |session: &mut Session, _| {
        for _ in 0..answered {
            session.answer(json!({ "status": "running", "running": true }));
        }
        qemu.lock().unwrap().kill().unwrap();
    })
}

fn shutdown_when_qemu_exits_after(home: &Path, answered: usize) {
    let world = world(home);
    let (server, _) = exits_after(&world, answered);
    let mut machine = world.attached();

    machine.shutdown().unwrap();

    server.join().unwrap();
    assert!(!world.marker.exists(), "the leftover was not removed");
    assert!(!world.lock_file().exists(), "the lock file was not removed");
}

#[skuld::test]
fn shutdown_is_done_when_qemu_exits_before_the_first_command(#[fixture(unicode_home)] home: &Path) {
    shutdown_when_qemu_exits_after(home, 0);
}

#[skuld::test]
fn shutdown_is_done_when_qemu_exits_after_the_status_query(#[fixture(unicode_home)] home: &Path) {
    shutdown_when_qemu_exits_after(home, 1);
}

#[skuld::test]
fn shutdown_of_a_machine_that_is_gone_removes_its_leftovers(#[fixture(unicode_home)] home: &Path) {
    let world = world(home);
    let mut machine = world.attached();
    world.end_qemu();

    machine.shutdown().unwrap();

    assert!(!world.marker.exists(), "the leftover was not removed");
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
    assert!(world.marker.exists(), "leftovers of a live machine were removed");
    drop(machine);
    server.join().unwrap();
    world.end_qemu();
}

#[skuld::test]
fn reset_of_a_launched_machine_asks_the_host_before_it_connects(#[fixture(unicode_home)] home: &Path) {
    let world = world(home);
    let mut machine = world.launched(world.dir.join("qemu.log"), &NO_GUEST_RESET);
    // Nothing listens and the VM is gone: a reset that connected would fail with `ConnectionLost`, not `Unavailable`.
    world.end_qemu();

    let error = machine.reset().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert_eq!(error.message(), WINDOWS_11);
}

#[skuld::test]
fn reset_of_an_attached_machine_sends_only_the_target_query_before_the_host_answers(
    #[fixture(unicode_home)] home: &Path,
) {
    let world = world(home);
    let (server, log) = serve(world.sock.listen(), 1, |session, _| {
        session.answer(json!({ "arch": "x86_64" }));
        assert!(session.command().is_none(), "a command followed the target query");
    });
    let mut machine = QemuMachine::attached(
        world.handle(),
        world.record.clone(),
        &NO_GUEST_RESET,
        Arc::new(MemoryProgress::default()),
    );

    let error = machine.reset().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert_eq!(error.message(), WINDOWS_11);
    drop(machine);
    server.join().unwrap();
    assert_eq!(*log.lock().unwrap(), ["qmp_capabilities"]);
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
