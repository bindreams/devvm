use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::Stdio;

use cosca::identity::ProcessIdRecord;

use crate::contain::{Lifetime, VmProcess};
use crate::test_support::fake_vm::{self, STARTED, connect_when_up, free_port, refuses};
use crate::test_support::fixtures::anna_home;
use crate::test_support::helper;
use crate::{ErrorKind, host};

fn frozen(home: &Path, lifetime: Lifetime) -> (VmProcess, u16) {
    let port = free_port();
    let vm = VmProcess::spawn_frozen(host::current(), fake_vm::command(port, home), lifetime)
        .expect("the fake VM should spawn frozen");
    (vm, port)
}

/// Starts helper `name` (`contain-owner` or `release-owner`) with a fake VM on `port`; stdout and stdin are piped.
fn owner(name: &str, port: u16, home: &Path) -> std::process::Child {
    helper::command(name, &[&port.to_string(), &home.display().to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the owner helper should start")
}

fn first_record(owner: &mut std::process::Child) -> ProcessIdRecord {
    let mut line = String::new();
    BufReader::new(owner.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("the owner printed no identity record ({e}): {line:?}"))
}

#[skuld::test]
fn a_frozen_vm_has_not_run_until_it_is_resumed(#[fixture(anna_home)] home: &Path) {
    let (mut vm, port) = frozen(home, Lifetime::DiesWithOwner);
    assert!(vm.is_alive().unwrap(), "a frozen VM exists");
    assert!(refuses(port), "a frozen VM listens on nothing");
    assert!(!home.join(STARTED).exists());

    vm.resume().unwrap();
    assert!(connect_when_up(&vm, port).starts_with("up "));
    assert!(home.join(STARTED).exists());
    vm.kill().unwrap();
}

#[skuld::test]
fn dropping_a_frozen_vm_kills_it_and_it_never_ran(#[fixture(anna_home)] home: &Path) {
    let (vm, port) = frozen(home, Lifetime::OutlivesOwner);
    let record = vm.record().unwrap();
    drop(vm);

    // The process may still be dying when `drop` returns; this waits for the real condition.
    if let Some(dying) = VmProcess::attach(&record).unwrap() {
        dying.wait().unwrap();
    }
    assert!(VmProcess::attach(&record).unwrap().is_none());
    assert!(!home.join(STARTED).exists(), "a dropped frozen VM never ran");
    assert!(refuses(port));
}

#[skuld::test]
fn a_failed_resume_leaves_no_process(#[fixture(anna_home)] home: &Path) {
    let (mut vm, port) = frozen(home, Lifetime::OutlivesOwner);
    // Something else kills the frozen process before the resume.
    let mut other = VmProcess::attach(&vm.record().unwrap()).unwrap().unwrap();
    other.kill().unwrap();

    let error = vm.resume().expect_err("a dead process cannot be resumed");
    assert!(!error.message().is_empty());
    assert!(!vm.is_alive().unwrap());
    assert!(!home.join(STARTED).exists());
    assert!(refuses(port));
}

#[skuld::test]
fn release_before_resume_is_an_internal_error(#[fixture(anna_home)] home: &Path) {
    let (mut vm, _) = frozen(home, Lifetime::OutlivesOwner);
    assert_eq!(vm.release().unwrap_err().kind(), ErrorKind::Internal);
    vm.resume().unwrap();
    vm.release().unwrap();
    assert_eq!(
        vm.release().unwrap_err().kind(),
        ErrorKind::Internal,
        "a second release is a mistake"
    );
    vm.kill().unwrap();
}

#[skuld::test]
fn resume_twice_is_an_internal_error(#[fixture(anna_home)] home: &Path) {
    let (mut vm, _) = frozen(home, Lifetime::DiesWithOwner);
    vm.resume().unwrap();
    assert_eq!(vm.resume().unwrap_err().kind(), ErrorKind::Internal);
    vm.kill().unwrap();
}

#[skuld::test]
fn a_contained_vm_dies_when_its_owner_is_killed(#[fixture(anna_home)] home: &Path) {
    let mut owner = owner("contain-owner", free_port(), home);
    let record = first_record(&mut owner);
    let vm = VmProcess::attach(&record)
        .unwrap()
        .expect("the VM runs while its owner lives");

    owner.kill().unwrap();
    owner.wait().unwrap();
    // Blocks until the job's close has killed the VM; a regression hangs here.
    vm.wait().unwrap();
    assert!(!vm.is_alive().unwrap());
}

#[skuld::test]
fn a_released_vm_survives_its_owner_exiting(#[fixture(anna_home)] home: &Path) {
    let port = free_port();
    let mut owner = owner("release-owner", port, home);
    let record = first_record(&mut owner);
    assert!(owner.wait().unwrap().success());

    let mut vm = VmProcess::attach(&record)
        .unwrap()
        .expect("the released VM outlives its owner");
    assert!(connect_when_up(&vm, port).starts_with("up "));
    vm.kill().unwrap();
}

#[skuld::test]
fn a_released_vm_does_not_hold_the_owners_pipes(#[fixture(anna_home)] home: &Path) {
    let mut owner = owner("release-owner", free_port(), home);
    let mut output = String::new();
    // EOF arrives only when every holder of the pipe's write end is gone: the owner, and the VM if it inherited it.
    owner.stdout.take().unwrap().read_to_string(&mut output).unwrap();
    let record: ProcessIdRecord = serde_json::from_str(&output).unwrap();
    owner.wait().unwrap();

    let mut vm = VmProcess::attach(&record)
        .unwrap()
        .expect("the VM is still running at EOF");
    vm.kill().unwrap();
}

#[skuld::test]
fn kill_of_a_running_vm_returns_only_after_exit(#[fixture(anna_home)] home: &Path) {
    let (mut vm, port) = frozen(home, Lifetime::DiesWithOwner);
    vm.resume().unwrap();
    connect_when_up(&vm, port);
    let record = vm.record().unwrap();

    vm.kill().unwrap();
    assert!(
        !vm.is_alive().unwrap(),
        "kill must not return before the process has exited"
    );
    assert!(VmProcess::attach(&record).unwrap().is_none());
    assert!(refuses(port));
    vm.kill().expect("killing a dead VM is Ok");
}

#[skuld::test]
fn an_unreleased_vm_reports_kill_on_close_and_a_released_one_does_not(#[fixture(anna_home)] home: &Path) {
    let (mut vm, port) = frozen(home, Lifetime::OutlivesOwner);
    vm.resume().unwrap();
    assert!(connect_when_up(&vm, port).contains("kill-on-close=true"));

    vm.release().unwrap();
    assert!(connect_when_up(&vm, port).contains("kill-on-close=false"));
    vm.kill().unwrap();
}

#[skuld::test]
fn an_outlives_owner_vm_has_no_console(#[fixture(anna_home)] home: &Path) {
    let (mut vm, port) = frozen(home, Lifetime::OutlivesOwner);
    vm.resume().unwrap();
    assert!(connect_when_up(&vm, port).ends_with("console=0"));
    vm.kill().unwrap();
}

#[skuld::test]
fn a_dies_with_owner_vm_has_no_console_either(#[fixture(anna_home)] home: &Path) {
    let (mut vm, port) = frozen(home, Lifetime::DiesWithOwner);
    vm.resume().unwrap();
    assert!(connect_when_up(&vm, port).ends_with("console=0"));
    vm.kill().unwrap();
}
