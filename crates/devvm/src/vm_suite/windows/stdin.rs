//! The stated limit of large piped stdin, and the listener that the base's ed25519-only host key keeps out of it.
//!
//! Large input piped through the system `ssh` can hang that session in the guest (README limits); `devvm stop` and `devvm
//! start` recover it. The first cases check the limit's bound: a client that is killed in the middle of such a transfer
//! ends with its session on the client's side and leaves the guest accepting. They do not assert that the transfer ends.
//! The last two check that back-to-back connections, which each make sshd's listener write its configuration and host
//! keys to a new session's pipe, leave the listener accepting. Every case boots an instance of its own: a wedged guest
//! would fail every case after it.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;

use super::machine::Client;
use super::session::{answer, ssh_command, ssh_remote};
use super::{FACTS, WindowsBase, facts, joined, ran, user_facts, windows_base, windows_options};
use crate::vm_suite::fixture::Up;
use crate::vm_suite::waits::{ANSWER_BOUND, awaiting, step};
use crate::{Command, ExitStatus, RunAs};

// A client killed in the middle of stdin ==============================================================================

/// The bytes of one write of the input.
const CHUNK: usize = 64 * 1024;

/// What the test has written into the client's stdin pipe when it kills the client. The guest's sshd advertises a 2 MiB
/// window, so the client takes this much whatever the guest does with it. A multiple of [`CHUNK`]: the count passes it
/// exactly once.
const TAKEN: u64 = 1 << 20;
const _: () = assert!(TAKEN.is_multiple_of(CHUNK as u64));

/// The line that a reader writes before it reads: it ran, and it is about to read.
const READING: &str = "devvm-reading";

/// 64 KiB of text lines, which every reader of the cases takes.
fn chunk() -> Vec<u8> {
    let line = [vec![b'x'; 1023], vec![b'\n']].concat();
    line.repeat(CHUNK / line.len())
}

/// Reads `pipe` to its end. Sends on `reading` for each line that is [`READING`]. Returns what it read.
fn read_marked(pipe: impl Read, reading: mpsc::Sender<()>) -> String {
    let mut pipe = BufReader::new(pipe);
    let mut text = String::new();
    let mut line = Vec::new();
    while matches!(pipe.read_until(b'\n', &mut line), Ok(n) if n > 0) {
        let line_text = String::from_utf8_lossy(&line);
        if line_text.trim_end() == READING {
            let _ = reading.send(());
        }
        text.push_str(&line_text);
        line.clear();
    }
    text
}

/// Pipes input without an end through the system `ssh` to `reader`, a command of the guest that writes [`READING`] and
/// then reads its stdin. It kills the client once its stdin pipe has taken [`TAKEN`] bytes (a count of what the test
/// wrote, not a time), and checks that a fresh command of the guest is answered. A hang of the session that the killed
/// client had is not a failure: the limit is stated.
///
/// `reader` is sent as it is: that is what a user's own `ssh -F` sends, and the guest's agent runs it as `pwsh -c
/// <reader>` with the session's stdin pipe. devvm's own wrapper is not in that path (`devvm run` sends no stdin).
fn a_killed_client_leaves_the_guest_accepting(base: &WindowsBase, name: &str, reader: &str) {
    step("booting an instance");
    let up = Up::new(&base.image, name, &windows_options()).unwrap();
    let config = up.ssh_config().unwrap();
    let mut command = ssh_command(&config, up.name(), reader);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut client = Client(command.spawn().unwrap_or_else(|e| panic!("cannot run ssh: {e}")));
    let mut input = client.0.stdin.take().expect("a piped stdin");
    let output = client.0.stdout.take().expect("a piped stdout");
    let mut errors = client.0.stderr.take().expect("a piped stderr");

    step("piping stdin through ssh until it has taken 1 MiB");
    let (reading, started) = mpsc::channel();
    let (taken, reached) = mpsc::channel();
    let (started, reached, ended, written, stdout, stderr) = std::thread::scope(|scope| {
        // The input has no end: the writer ends when the client does, by its failed write.
        let writer = scope.spawn(move || {
            let chunk = chunk();
            let mut total = 0_u64;
            loop {
                if let Err(e) = input.write_all(&chunk) {
                    return (total, e);
                }
                total += chunk.len() as u64;
                if total == TAKEN {
                    let _ = taken.send(());
                }
            }
        });
        // The client's output is read for the whole run, so that a full pipe never stops it from reading stdin.
        let stdout = scope.spawn(move || read_marked(output, reading));
        let stderr = scope.spawn(move || {
            let mut bytes = Vec::new();
            let _ = errors.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        });
        let watch = awaiting(
            "the reader's first line and the ssh client's stdin taking 1 MiB",
            ANSWER_BOUND,
        );
        let started = started.recv();
        let reached = reached.recv();
        drop(watch);
        let ended = client.0.try_wait();
        // On every path: the threads end with their client, so the scope can end.
        let _ = client.0.kill();
        let _ = client.0.wait();
        (started, reached, ended, joined(writer), joined(stdout), joined(stderr))
    });
    let (total, error) = written;
    let seen =
        format!("ssh {ended:?}; {total} bytes of stdin written, then: {error}; stdout {stdout:?}; stderr {stderr:?}");
    assert!(started.is_ok(), "the reader did not write {READING}: {seen}");
    assert!(reached.is_ok(), "ssh took less than 1 MiB of stdin: {seen}");
    assert!(
        matches!(ended, Ok(None)),
        "ssh ended by itself within the transfer: {seen}"
    );

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
        &format!(
            "pwsh -NoProfile -NonInteractive -Command '[Console]::Out.WriteLine(''{READING}''); $input | Out-Null'"
        ),
    );
}

#[skuld::test]
fn windows_a_client_killed_mid_stdin_to_windows_powershell_leaves_the_guest_accepting(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    a_killed_client_leaves_the_guest_accepting(
        base,
        "w-stdin-ps51",
        &format!(
            "powershell.exe -NoProfile -NonInteractive -Command '[Console]::Out.WriteLine(''{READING}''); [Console]::In.ReadToEnd() | Out-Null'"
        ),
    );
}

/// `find` is a plain program of the system: it reads its stdin with `ReadFile`, where the two above read it through a
/// .NET runtime. It cannot write [`READING`] itself, so the command writes it before `find` starts.
#[skuld::test]
fn windows_a_client_killed_mid_stdin_to_a_readfile_program_leaves_the_guest_accepting(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    a_killed_client_leaves_the_guest_accepting(
        base,
        "w-stdin-readfile",
        &format!("[Console]::Out.WriteLine('{READING}'); find /c /v ''"),
    );
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
