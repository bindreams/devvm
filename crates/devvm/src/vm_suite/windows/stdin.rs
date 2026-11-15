//! The stated limit of large piped stdin, and the listener that the base's ed25519-only host key keeps out of it.
//!
//! Large input piped through the system `ssh` can hang that session in the guest (README limits); `devvm stop` and `devvm
//! start` recover it. The first cases check the limit's bound: a client that is killed in the middle of such a transfer
//! ends with its session on the client's side and leaves the guest accepting. They do not assert that the transfer ends.
//! They test the path of a user's own `ssh -F <config> <name> <command>` with the generated configuration, whose command
//! the guest's agent runs as `pwsh -c <command>` with the session's stdin pipe. `devvm ssh <name> -- <command>` is not
//! that path: it runs the command with the embedded client and no stdin. The last two cases check that back-to-back connections, which
//! each make sshd's listener write its configuration and host keys to a new session's pipe, leave the listener
//! accepting. Every case boots an instance of its own: a wedged guest would fail every case after it.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;

use super::machine::Client;
use super::session::{answer, ssh_command, ssh_remote};
use super::{FACTS, PACE, WindowsBase, facts, joined, pwsh, ran, user_facts, windows_base, windows_options};
use crate::vm_suite::fixture::Up;
use crate::vm_suite::waits::{ANSWER_BOUND, awaiting, step};
use crate::{Command, ExitStatus, RunAs, random};

// A client killed in the middle of stdin ==============================================================================

/// The bytes of one write of the input.
const CHUNK: usize = 64 * 1024;

/// What the test has written into the client's stdin pipe when it kills the client. The window that the guest's sshd
/// advertises is 2 MiB, so the client takes this much whatever the guest does with it. A multiple of [`CHUNK`]: the count
/// passes it exactly once.
const TAKEN: u64 = 1 << 20;
const _: () = assert!(TAKEN.is_multiple_of(CHUNK as u64));

/// What a reader's token starts with: a fresh nonce follows it, so that only this case's reader has the token.
const TOKEN: &str = "devvm-reader-";

/// 64 KiB of text lines, which every reader of the cases takes.
fn chunk() -> Vec<u8> {
    let line = [vec![b'x'; 1023], vec![b'\n']].concat();
    let chunk = line.repeat(CHUNK / line.len());
    assert_eq!(chunk.len(), CHUNK, "the count of written bytes passes TAKEN exactly");
    chunk
}

/// Reads `pipe` (a stream of the ssh client) line by line until it ends, logs each line as it arrives, so that a case
/// that ends at its bound shows it, and returns all of it. A read error ends the stream: the client's writes then fail,
/// they do not block.
fn follow(stream: &str, pipe: impl Read) -> String {
    let mut pipe = BufReader::new(pipe);
    let mut text = String::new();
    let mut line = Vec::new();
    loop {
        let ended = !matches!(pipe.read_until(b'\n', &mut line), Ok(read) if read > 0);
        if !line.is_empty() {
            let part = String::from_utf8_lossy(&line);
            eprintln!("ssh {stream}: {}", part.trim_end());
            text.push_str(&part);
            line.clear();
        }
        if ended {
            return text;
        }
    }
}

/// The names of the programs of the guest that are the readers whose command line has [`TOKEN`] and `nonce`, one per
/// line. A reader is the program that the `pwsh -c` of the agent started for the command: it has the token, and its
/// parent is a `pwsh.exe` that has it. The token is written in two parts, so that this script's own command line does
/// not have it.
fn readers_on_the_guest(nonce: &str) -> Command {
    pwsh(
        RunAs::User,
        &format!(
            "$token = '{TOKEN}' + '{nonce}'; \
             $hits = @(Get-CimInstance Win32_Process | Where-Object {{ $_.CommandLine -and $_.CommandLine.Contains($token) }}); \
             $shells = @($hits | Where-Object {{ $_.Name -eq 'pwsh.exe' }} | ForEach-Object {{ $_.ProcessId }}); \
             $hits | Where-Object {{ $shells -contains $_.ParentProcessId }} | ForEach-Object {{ $_.Name }}"
        ),
    )
}

/// Pipes input without an end through the system `ssh` to `reader(token)`, a command of the guest that reads its stdin
/// and has `token` in its command line, and which runs as the program `reader_exe` there. It kills the client once its
/// stdin pipe has taken [`TAKEN`] bytes (a count of what the test wrote, not a time) and fresh commands of the guest,
/// asked again until one finds it, have seen the reader alive: a reader that has not failed is alive, since its stdin
/// never closes. The looks run before the kill, while the client's session may hang: a guest that stops answering
/// other sessions then contradicts the stated limit, a hang of that session only, so the case fails. Then it checks that
/// a fresh command of the guest is answered. A hang of the session that the killed client had is not a
/// failure: the limit is stated.
///
/// `reader` is sent as it is: that is what a user's own `ssh -F` sends. devvm's own wrapper is not in that path
/// (`devvm run` sends no stdin).
fn a_killed_client_leaves_the_guest_accepting(
    base: &WindowsBase,
    name: &str,
    reader: impl FnOnce(&str) -> String,
    reader_exe: &str,
) {
    step("booting an instance");
    let up = Up::new(&base.image, name, &windows_options()).unwrap();
    let config = up.ssh_config().unwrap();
    let nonce = hex::encode(random::bytes::<8>());
    let token = format!("{TOKEN}{nonce}");
    let mut command = ssh_command(&config, up.name(), &reader(&token));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut client = Client(command.spawn().unwrap_or_else(|e| panic!("cannot run ssh: {e}")));
    let mut input = client.0.stdin.take().expect("a piped stdin");
    let stdout = client.0.stdout.take().expect("a piped stdout");
    let stderr = client.0.stderr.take().expect("a piped stderr");

    step("piping stdin through ssh until it has taken 1 MiB");
    let (taken, reached) = mpsc::channel();
    let looking = readers_on_the_guest(&nonce);
    let (reached, look, written, stdout, stderr) = std::thread::scope(|scope| {
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
        let stdout = scope.spawn(move || follow("stdout", stdout));
        let stderr = scope.spawn(move || follow("stderr", stderr));
        let watch = awaiting("the ssh client's stdin taking 1 MiB", ANSWER_BOUND);
        let reached = reached.recv();
        drop(watch);
        // A failed look is a result, not a panic: a panic here would leave the writer blocked and the scope never ending.
        let look = reached.is_ok().then(|| {
            step("looking for the reader on the guest");
            // Nothing orders the reader's start before the first look, so the looks go on until the reader shows. The
            // bound is the failure: a reader that never shows (it failed to start, or died) is not an answer.
            let _watch = awaiting(
                format!("a look finding {reader_exe} with {token} in its command line on the guest"),
                ANSWER_BOUND,
            );
            loop {
                // An ended client closed the reader's stdin: the reader can never show, so the case fails now.
                if let Ok(Some(status)) = client.0.try_wait() {
                    break Err(format!("ssh ended with {status} before the reader showed"));
                }
                let output = match ran(&*up, "a look at the reader on the guest", &looking) {
                    Ok(output) => output,
                    Err(e) => break Err(format!("cannot look at the reader on the guest: {e}")),
                };
                let readers = output.stdout_lossy();
                if output.status != ExitStatus::Code(0) {
                    break Err(format!(
                        "the look at the reader failed: {:?} {readers:?} {}",
                        output.status,
                        output.stderr_lossy()
                    ));
                }
                eprintln!("the look found {readers:?}");
                if readers
                    .lines()
                    .any(|program| program.trim().eq_ignore_ascii_case(reader_exe))
                {
                    break Ok(());
                }
                std::thread::sleep(PACE);
            }
        });
        // On every path: the threads end with their client, so the scope can end. None of these ends by itself if the
        // client does not.
        let _ending = awaiting("the ssh client and its pipes ending after the kill", ANSWER_BOUND);
        let _ = client.0.kill();
        let _ = client.0.wait();
        (reached, look, joined(writer), joined(stdout), joined(stderr))
    });
    let (total, error) = written;
    let seen = format!("{total} bytes of stdin written, then: {error}; stdout {stdout:?}; stderr {stderr:?}");
    assert!(reached.is_ok(), "ssh took less than 1 MiB of stdin: {seen}");
    look.expect("a look when the client took 1 MiB")
        .unwrap_or_else(|e| panic!("{e}; {seen}"));

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
        |token| format!("pwsh -NoProfile -NonInteractive -Command '$input | Out-Null # {token}'"),
        "pwsh.exe",
    );
}

#[skuld::test]
fn windows_a_client_killed_mid_stdin_to_windows_powershell_leaves_the_guest_accepting(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    a_killed_client_leaves_the_guest_accepting(
        base,
        "w-stdin-ps51",
        |token| {
            format!(
                "powershell.exe -NoProfile -NonInteractive -Command '[Console]::In.ReadToEnd() | Out-Null # {token}'"
            )
        },
        "powershell.exe",
    );
}

/// `find` is a plain program of the system: it reads its stdin with `ReadFile`, where the two above read it through a
/// .NET runtime. The token is its search string. The stop-parsing token `--%` makes PowerShell pass the rest of the line
/// as it is: without it the quotes around the search string are stripped and `find` ends at once ("Parameter format not
/// correct").
#[skuld::test]
fn windows_a_client_killed_mid_stdin_to_a_readfile_program_leaves_the_guest_accepting(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    a_killed_client_leaves_the_guest_accepting(
        base,
        "w-stdin-readfile",
        |token| format!("find --% /c /v \"{token}\""),
        "find.exe",
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
