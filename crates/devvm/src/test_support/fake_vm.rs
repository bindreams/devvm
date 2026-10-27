//! The fake VM: a real process that stands in for QEMU. It listens on a loopback port of its own choosing, writes a
//! `started` file naming that port when it first runs, and answers each connection with one line describing its own
//! containment. A client that then sends [`SHUTDOWN_REQUEST`] makes it exit by itself.
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

/// The file a fake VM writes when it is asked to shut down (see [`SHUTDOWN_REQUEST`]), just before it exits; a killed
/// one never does.
pub const GRACEFUL: &str = "stopped-gracefully";

/// What a client sends to the fake VM to ask it to shut down.
pub const SHUTDOWN_REQUEST: &str = "shutdown";

/// Where the port is written before it is renamed to [`STARTED`].
const PENDING: &str = "started.tmp";

/// What a fake VM writes to its stdout and stderr; a test hands `log(state_dir)` to `spawn_frozen` to collect it.
pub const OUT_LINE: &str = "fake-vm: out";
pub const ERR_LINE: &str = "fake-vm: err";

/// Where the fake VM's output goes for the state directory `state_dir`.
pub fn log(state_dir: &Path) -> PathBuf {
    state_dir.join("fake-vm.log")
}

/// Deletes what an earlier VM of `state_dir` left, so that the `started` file describes only the run that follows. Call
/// it before spawning a VM into a directory that may have held one; a VM that is not spawned yet cannot write the file.
pub fn forget_earlier_runs(state_dir: &Path) -> std::io::Result<()> {
    for name in [STARTED, PENDING, GRACEFUL] {
        match std::fs::remove_file(state_dir.join(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// The command that runs the fake VM, with `state_dir` for its `started` file.
pub fn command(state_dir: &Path) -> cosca::Command {
    helper::cosca_command("fake-vm", &[&state_dir.display().to_string()])
}

/// Helper `fake-vm <state-dir>`. Binds a loopback port of its own choosing, like QEMU's port-0 forward, and reports it
/// as the content of `started`, which appears (whole, by rename) only once the VM listens.
pub fn fake_vm(args: &[String]) {
    let state_dir = PathBuf::from(&args[0]);
    // Before the port opens, so a connection proves both lines were written.
    println!("{OUT_LINE}");
    eprintln!("{ERR_LINE}");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port should be available");
    let port = listener.local_addr().expect("a bound listener has an address").port();
    let pending = state_dir.join(PENDING);
    std::fs::write(&pending, port.to_string()).expect("the state directory should exist");
    std::fs::rename(&pending, state_dir.join(STARTED)).expect("the state directory should exist");
    for stream in listener.incoming() {
        let mut stream = stream.expect("an accepted connection");
        let _ = writeln!(
            stream,
            "up pid={} kill-on-close={} console={}",
            std::process::id(),
            probe::kill_on_close(),
            probe::console_processes()
        );
        // A client may then ask the VM to power off, as a backend asks its guest to.
        let mut request = String::new();
        let _ = BufReader::new(&stream).read_line(&mut request);
        if request.trim_end() == SHUTDOWN_REQUEST {
            std::fs::write(state_dir.join(GRACEFUL), b"").expect("the state directory should exist");
            std::process::exit(0);
        }
    }
}

/// Waits until the fake VM of `state_dir` listens and returns its port. Fails if the process dies first, so a fake VM
/// that never comes up fails the test and does not hang it.
pub fn await_port(vm: &VmProcess, state_dir: &Path) -> u16 {
    let started = state_dir.join(STARTED);
    loop {
        if let Ok(text) = std::fs::read_to_string(&started) {
            return text
                .parse()
                .unwrap_or_else(|e| panic!("{} holds {text:?}: {e}", started.display()));
        }
        if !vm.is_alive().unwrap() {
            // It may have written the file between the read and the liveness check.
            if let Ok(text) = std::fs::read_to_string(&started) {
                return text.parse().expect("the started file holds a port");
            }
            panic!("the fake VM in {} exited before it listened", state_dir.display());
        }
        std::thread::yield_now();
    }
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

/// Connects once to the fake VM on `port` and returns its answer line. A VM whose `started` file names the port
/// already listens, so a refused connection fails the test: the port belongs to nothing that runs.
pub fn answer(port: u16) -> String {
    let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .unwrap_or_else(|e| panic!("nothing listens on port {port}, which a machine reported as its endpoint: {e}"));
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .expect("the fake VM answers");
    line.trim_end().to_owned()
}

/// Spawns the fake VM frozen with `lifetime`, resumes it and prints its identity record on one line.
fn spawn_and_report(args: &[String], lifetime: Lifetime) -> VmProcess {
    let state_dir = Path::new(&args[0]);
    let mut vm = VmProcess::spawn_frozen(host::current(), command(state_dir), lifetime, &log(state_dir))
        .expect("the fake VM should spawn frozen");
    vm.resume().expect("the fake VM should resume");
    vm
}

fn print_record(vm: &VmProcess) {
    let line = serde_json::to_string(&vm.record().expect("an identity record")).unwrap();
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{line}").and_then(|()| stdout.flush()).unwrap();
}

/// Helper `contain-owner <state-dir>`: owns a `DiesWithOwner` fake VM and parks until its stdin closes.
pub fn contain_owner(args: &[String]) {
    let vm = spawn_and_report(args, Lifetime::DiesWithOwner);
    print_record(&vm);
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
}

/// Helper `release-owner <state-dir>`: starts an `OutlivesOwner` fake VM, releases it, prints its identity and
/// exits.
pub fn release_owner(args: &[String]) {
    let mut vm = spawn_and_report(args, Lifetime::OutlivesOwner);
    vm.release().expect("the fake VM should release");
    print_record(&vm);
}

/// Helper `console-window`: prints whether this process has a console window and how many processes share its console.
pub fn console_window(_args: &[String]) {
    println!(
        "window={} console={}",
        probe::console_window(),
        probe::console_processes()
    );
}
