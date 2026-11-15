//! The stated limit of large piped stdin, and the listener that the base's ed25519-only host key keeps out of it.
//!
//! Large input piped through the system `ssh` can hang that session in the guest (README limits); `devvm stop` and `devvm
//! start` recover it. The first cases check the limit's bound: a client that is killed in the middle of such a transfer
//! ends with its session on the client's side and leaves the guest accepting. They do not assert that the transfer ends.
//! The last two check that back-to-back connections, which each make sshd's listener write its configuration and host
//! keys to a new session's pipe, leave the listener accepting. Every case boots an instance of its own: a wedged guest
//! would fail every case after it.

use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;

use super::elevation::joined;
use super::machine::Client;
use super::session::{answer, ssh_command, ssh_remote};
use super::{FACTS, WindowsBase, facts, ran, user_facts, windows_base, windows_options};
use crate::guest::GuestOs;
use crate::guest::windows::WindowsGuest;
use crate::vm_suite::fixture::Up;
use crate::vm_suite::waits::{ANSWER_BOUND, awaiting, step};
use crate::{Command, ExitStatus, RunAs};

// A client killed in the middle of stdin ==============================================================================

/// What the test has written into the client's stdin pipe when it kills the client. The client's channel window is
/// 2 MiB, so the client takes this much whatever the guest does with it.
const TAKEN: u64 = 1 << 20;

/// 64 KiB of text lines, which every reader of the cases takes.
fn chunk() -> Vec<u8> {
    let line = [vec![b'x'; 1023], vec![b'\n']].concat();
    line.repeat(64)
}

/// Pipes input without an end through the system `ssh` to `reader`, a program of the guest that reads its stdin, kills the
/// client once its stdin pipe has taken [`TAKEN`] bytes (a count of what the test wrote, not a time), and checks that a
/// fresh command of the guest is answered. A hang of the session that the killed client had is not a failure: the limit
/// is stated.
fn a_killed_client_leaves_the_guest_accepting(base: &WindowsBase, name: &str, reader: &[&str]) {
    step("booting an instance");
    let up = Up::new(&base.image, name, &windows_options()).unwrap();
    let config = up.ssh_config().unwrap();
    let remote = WindowsGuest.wrap(&Command::new(reader.iter().copied())).unwrap();
    let mut command = ssh_command(&config, up.name(), &remote);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut client = Client(command.spawn().unwrap_or_else(|e| panic!("cannot run ssh: {e}")));
    let mut input = client.0.stdin.take().expect("a piped stdin");

    step("piping stdin through ssh until it has taken 1 MiB");
    let (taken, reached) = mpsc::channel();
    let (reached, ended, written) = std::thread::scope(|scope| {
        // The input has no end: the writer ends when the client does, by its failed write.
        let writer = scope.spawn(move || {
            let chunk = chunk();
            let mut total = 0_u64;
            let mut told = false;
            loop {
                if let Err(e) = input.write_all(&chunk) {
                    return (total, e);
                }
                total += chunk.len() as u64;
                if total >= TAKEN && !told {
                    told = true;
                    let _ = taken.send(());
                }
            }
        });
        let watch = awaiting("the ssh client's stdin taking 1 MiB", ANSWER_BOUND);
        let reached = reached.recv();
        drop(watch);
        let ended = client.0.try_wait();
        // On every path: the writer ends with its client, so the scope can end.
        let _ = client.0.kill();
        let _ = client.0.wait();
        (reached, ended, joined(writer))
    });
    let (total, error) = written;
    let mut stderr = String::new();
    if let Some(mut pipe) = client.0.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    assert!(
        reached.is_ok(),
        "ssh took only {total} bytes of stdin before it ended ({error}): {stderr}"
    );
    assert!(
        matches!(ended, Ok(None)),
        "ssh ended by itself ({ended:?}) within the transfer, after {total} bytes; stdin ended with: {error}: {stderr}"
    );
    assert!(total >= TAKEN, "{total} bytes of stdin were written");

    step("running a command in the guest after the kill");
    assert_eq!(
        facts(&*up, RunAs::User),
        user_facts(),
        "the guest did not answer in the user's session after the client was killed"
    );
}

#[skuld::test]
fn windows_a_client_killed_mid_stdin_to_pwsh_leaves_the_guest_accepting(#[fixture(windows_base)] base: &WindowsBase) {
    a_killed_client_leaves_the_guest_accepting(
        base,
        "w-stdin-pwsh",
        &["pwsh", "-NoProfile", "-NonInteractive", "-Command", "$input | Out-Null"],
    );
}

#[skuld::test]
fn windows_a_client_killed_mid_stdin_to_windows_powershell_leaves_the_guest_accepting(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    a_killed_client_leaves_the_guest_accepting(
        base,
        "w-stdin-ps51",
        &[
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Console]::In.ReadToEnd() | Out-Null",
        ],
    );
}

/// `find` is a plain program of the system: it reads its input with `ReadFile`, and no runtime stands between it and the
/// pipe.
#[skuld::test]
fn windows_a_client_killed_mid_stdin_to_a_readfile_program_leaves_the_guest_accepting(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    a_killed_client_leaves_the_guest_accepting(base, "w-stdin-readfile", &["find", "/c", "/v", ""]);
}

// Back-to-back connections ============================================================================================

/// What M-d measured (run 43346095490): a burst is [`CLIENTS`] clients at once, each making [`PER_CLIENT`] connections
/// one after the other, so sessions end while another connection is made; [`BURSTS`] bursts, 400 connections. These are
/// the sample's size, not a limit on retries.
const BURSTS: usize = 20;
const CLIENTS: usize = 5;
const PER_CLIENT: usize = 4;

/// Makes the bursts of connections with `connect`, which answers whether one connection ran a command to its end with
/// status 0. A connection that is not answered ends the test process at the bound of the call that made it, naming it.
fn bursts(connect: impl Fn() -> Result<(), String> + Sync) {
    for burst in 1..=BURSTS {
        step(&format!("burst {burst} of {BURSTS}"));
        let failed: Vec<String> = std::thread::scope(|scope| {
            let clients: Vec<_> = (0..CLIENTS)
                .map(|_| scope.spawn(|| (0..PER_CLIENT).filter_map(|_| connect().err()).collect::<Vec<_>>()))
                .collect();
            clients.into_iter().flat_map(joined).collect()
        });
        assert!(
            failed.is_empty(),
            "burst {burst} of {BURSTS}: {} of {} connections failed: {failed:?}",
            failed.len(),
            CLIENTS * PER_CLIENT
        );
    }
}

#[skuld::test]
fn windows_back_to_back_sessions_keep_the_listener_accepting(#[fixture(windows_base)] base: &WindowsBase) {
    step("booting an instance");
    let up = Up::new(&base.image, "w-listener-exec", &windows_options()).unwrap();
    let command = Command::new(["whoami"]);

    bursts(|| {
        let output = ran(&*up, "a short command of a burst", &command).map_err(|e| e.to_string())?;
        match &output.status {
            ExitStatus::Code(0) => Ok(()),
            status => Err(format!("{status:?}: {}", output.stderr_lossy())),
        }
    });

    step("running a command after the bursts");
    assert_eq!(
        facts(&*up, RunAs::User),
        user_facts(),
        "the guest did not answer after the bursts"
    );
}

/// What a plain `ssh` makes of `remote`, as the one connection of a burst.
fn ssh_connection(config: &Path, name: &str, remote: &str) -> Result<(), String> {
    let output = ssh_remote(config, name, remote);
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[skuld::test]
fn windows_back_to_back_ssh_sessions_with_no_stdin_keep_the_listener_accepting(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    step("booting an instance");
    let up = Up::new(&base.image, "w-listener-ssh", &windows_options()).unwrap();
    let config = up.ssh_config().unwrap();

    bursts(|| ssh_connection(&config, up.name(), "exit 0"));

    step("running a command through ssh after the bursts");
    let output = ssh_remote(&config, up.name(), FACTS);
    assert_eq!(
        answer(&output).as_deref(),
        Some(user_facts().as_str()),
        "ssh after the bursts: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
