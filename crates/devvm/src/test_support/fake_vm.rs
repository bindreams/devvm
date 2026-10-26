//! The fake VM: a real process that stands in for QEMU. It listens on a loopback port, writes a `started` file when it
//! first runs, and answers each connection with one line describing its own containment.
//!
//! Only the tests that spawn a frozen process (Windows in 0.1.0) use most of this.
#![cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "used by tests that spawn a frozen VM, which only Windows can do"
    )
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use crate::contain::{Lifetime, VmProcess};
use crate::host;
use crate::test_support::helper;

#[cfg(windows)]
#[path = "fake_vm/probe_windows.rs"]
mod probe;
#[cfg(not(windows))]
#[path = "fake_vm/probe_other.rs"]
mod probe;

/// The file a fake VM writes when it runs; a frozen one never does.
pub const STARTED: &str = "started";

/// A port nobody listens on right now. Another process could take it before the fake VM does; the fake is test code,
/// so a lost race fails the test visibly.
pub fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .expect("a loopback port should be available")
        .port()
}

/// The command that runs the fake VM on `port`, with `state_dir` for its `started` file.
pub fn command(port: u16, state_dir: &Path) -> cosca::Command {
    helper::cosca_command("fake-vm", &[&port.to_string(), &state_dir.display().to_string()])
}

/// Helper `fake-vm <port> <state-dir>`.
pub fn fake_vm(args: &[String]) {
    let port: u16 = args[0].parse().expect("the port is a number");
    let state_dir = PathBuf::from(&args[1]);
    // Listening first, so that `started` means the port is open.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).expect("the port should be free");
    std::fs::write(state_dir.join(STARTED), b"").expect("the state directory should exist");
    for stream in listener.incoming() {
        let mut stream = stream.expect("an accepted connection");
        let _ = writeln!(
            stream,
            "up kill-on-close={} console={}",
            probe::kill_on_close(),
            probe::console_processes()
        );
    }
}

/// Whether nothing accepts connections on `port`.
pub fn refuses(port: u16) -> bool {
    TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err()
}

/// Connects to the fake VM and returns its answer line, retrying until it accepts. The retry ends with an error when
/// the process dies instead, so a fake VM that never comes up fails the test and does not hang it.
pub fn connect_when_up(vm: &VmProcess, port: u16) -> String {
    loop {
        match TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
            Ok(stream) => {
                let mut line = String::new();
                BufReader::new(stream)
                    .read_line(&mut line)
                    .expect("the fake VM answers");
                return line.trim_end().to_owned();
            }
            Err(e) => {
                assert!(
                    vm.is_alive().unwrap(),
                    "the fake VM on port {port} exited before it accepted a connection ({e})"
                );
                std::thread::yield_now();
            }
        }
    }
}

/// Spawns the fake VM frozen with `lifetime`, resumes it and prints its identity record on one line.
fn spawn_and_report(args: &[String], lifetime: Lifetime) -> VmProcess {
    let port: u16 = args[0].parse().expect("the port is a number");
    let mut vm = VmProcess::spawn_frozen(host::current(), command(port, Path::new(&args[1])), lifetime)
        .expect("the fake VM should spawn frozen");
    vm.resume().expect("the fake VM should resume");
    vm
}

fn print_record(vm: &VmProcess) {
    let line = serde_json::to_string(&vm.record().expect("an identity record")).unwrap();
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{line}").and_then(|()| stdout.flush()).unwrap();
}

/// Helper `contain-owner <port> <state-dir>`: owns a `DiesWithOwner` fake VM and parks until its stdin closes.
pub fn contain_owner(args: &[String]) {
    let vm = spawn_and_report(args, Lifetime::DiesWithOwner);
    print_record(&vm);
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
}

/// Helper `release-owner <port> <state-dir>`: starts an `OutlivesOwner` fake VM, releases it, prints its identity and
/// exits.
pub fn release_owner(args: &[String]) {
    let mut vm = spawn_and_report(args, Lifetime::OutlivesOwner);
    vm.release().expect("the fake VM should release");
    print_record(&vm);
}

/// Helper `console-window`: prints whether this process has a console window and how many processes share its console.
pub fn console_window(_args: &[String]) {
    println!("window={} console={}", probe::console_window(), probe::console_processes());
}
