//! What `QemuMachine::reset` decides before and after it talks to QEMU, with a fake host and a fake QMP server and no
//! QEMU process: the order host, `-no-reboot`, `query-target`.
//!
//! The machine's process is the test process itself (a handle on it, nothing spawned). A refusal that comes before QEMU
//! is asked must not consult the process watch at all, so those tests give the machine a watch that fails when
//! consulted; a regression then fails the assertion on the error instead of waiting on a process that is not there.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;

use super::{NO_REBOOT_REFUSAL, QemuMachine};
use crate::backend::qemu::qmp::qmp_tests::serve;
use crate::backend::{Machine, MachineRecord};
use crate::contain::VmProcess;
use crate::host::{self, Host, ProcessWatch};
use crate::test_support::fixtures::MemoryProgress;
use crate::test_support::host::{NO_GUEST_RESET, WINDOWS_11};
use crate::test_support::manual_exit::ManualExit;
use crate::test_support::qemu::Sock;
use crate::test_support::refusing::Refusing;
use crate::{Arch, ErrorKind};

/// A machine record naming the test process, whose control socket is `sock`.
fn record(sock: &Sock, no_reboot: bool) -> (VmProcess, MachineRecord) {
    let cosca::identity::Resolved::Found(process) = cosca::Process::from_pid(std::process::id()) else {
        panic!("the test process cannot be identified");
    };
    let record = MachineRecord {
        process: process.id().to_record().unwrap(),
        qmp: sock.endpoint.path().display().to_string(),
        ssh: Vec::new(),
        serial_log: sock.endpoint.path().with_file_name("serial.log"),
        leftovers: Vec::new(),
        no_reboot,
    };
    let vm = VmProcess::attach(&record.process)
        .unwrap()
        .expect("the test process runs");
    (vm, record)
}

/// A machine launched on `host` that was resumed, whose process watch is `watch`.
fn launched(
    sock: &Sock,
    no_reboot: bool,
    host: &'static dyn Host,
    arch: Arch,
    watch: Arc<dyn ProcessWatch>,
) -> QemuMachine {
    let (vm, record) = record(sock, no_reboot);
    let mut machine = QemuMachine::launched(
        vm,
        watch,
        record,
        Vec::new(),
        PathBuf::from("qemu.log"),
        arch,
        host,
        Arc::new(MemoryProgress::default()),
    );
    machine.resumed = true;
    machine
}

fn attached(sock: &Sock, no_reboot: bool, host: &'static dyn Host, watch: Arc<dyn ProcessWatch>) -> QemuMachine {
    let (vm, record) = record(sock, no_reboot);
    QemuMachine::attached(vm, watch, record, host, Arc::new(MemoryProgress::default())).unwrap()
}

/// A watch that fails the test's assertion when anything connects to QEMU.
fn tripwire() -> Arc<dyn ProcessWatch> {
    let watch = Refusing::new(ManualExit::new().watch());
    watch.refuse_has_exited();
    watch.watch()
}

#[skuld::test]
fn the_no_reboot_refusal_is_one_sentence_that_names_the_cause() {
    assert!(NO_REBOOT_REFUSAL.contains("no-reboot"), "{NO_REBOOT_REFUSAL}");
    assert!(NO_REBOOT_REFUSAL.contains("resetting the guest"), "{NO_REBOOT_REFUSAL}");
    assert!(
        !NO_REBOOT_REFUSAL.contains("  "),
        "a lost line continuation: {NO_REBOOT_REFUSAL}"
    );
}

#[skuld::test]
fn reset_of_a_launched_machine_asks_the_host_before_it_connects() {
    let sock = Sock::new();
    let mut machine = launched(&sock, false, &NO_GUEST_RESET, Arch::Amd64, tripwire());

    let error = machine.reset().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert_eq!(error.message(), WINDOWS_11);
}

#[skuld::test]
fn reset_of_a_launched_no_reboot_machine_is_refused_after_the_host_and_before_it_connects() {
    let sock = Sock::new();
    // aarch64 is a guest every host can reset, so it is the no-reboot check that refuses.
    let mut machine = launched(&sock, true, host::current(), Arch::Arm64, tripwire());

    let error = machine.reset().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert_eq!(error.message(), NO_REBOOT_REFUSAL);
}

#[skuld::test]
fn reset_of_a_launched_no_reboot_machine_on_a_host_that_cannot_reset_names_the_host() {
    let sock = Sock::new();
    let mut machine = launched(&sock, true, &NO_GUEST_RESET, Arch::Amd64, tripwire());

    let error = machine.reset().unwrap_err();

    assert_eq!(error.message(), WINDOWS_11);
}

#[skuld::test]
fn reset_of_an_attached_no_reboot_machine_is_refused_before_it_asks_qemu_anything() {
    let sock = Sock::new();
    let mut machine = attached(&sock, true, host::current(), tripwire());

    let error = machine.reset().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert_eq!(error.message(), NO_REBOOT_REFUSAL);
}

#[skuld::test]
fn reset_of_an_attached_machine_sends_only_the_target_query_before_the_host_answers() {
    let sock = Sock::new();
    let (server, log) = serve(sock.listen(), 1, |session, _| {
        session.answer(json!({ "arch": "x86_64" }));
        assert!(session.command().is_none(), "a command followed the target query");
    });
    let mut machine = attached(&sock, false, &NO_GUEST_RESET, ManualExit::new().watch());

    let error = machine.reset().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert_eq!(error.message(), WINDOWS_11);
    drop(machine);
    server.join().unwrap();
    assert_eq!(*log.lock().unwrap(), ["qmp_capabilities"]);
}
