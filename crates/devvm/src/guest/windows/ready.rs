//! Telling that a Windows guest's user session is up: a command that runs there, at Medium integrity.
//!
//! sshd answers before the agent of ssh-broker exists (the boot task starts it after the agent, but the user's session
//! is what a command needs), and after `shutdown /r` the old boot answers from session 0 at High until it goes down.
//! The probe is an ordinary wrapped command, so a plain session answers it with 126 (the wrapper's own session check)
//! and nothing runs; one that reaches the user's session reports the session and the integrity level it runs at.

use super::wrap::{NOT_IN_SESSION, wrap};
use crate::access::SshSession;
use crate::exit::{ExitStatus, Output};
use crate::guest::Readiness;
use crate::{Command, Error, ErrorKind, Result};

#[cfg(test)]
#[path = "ready_tests.rs"]
mod ready_tests;

#[cfg(all(test, windows))]
#[path = "ready_pwsh_tests.rs"]
mod ready_pwsh_tests;

/// The first word of the probe's answer. Only digits and SIDs follow it: nothing localized is read.
pub(crate) const READY_PROBE: &str = "DEVVM-READY";

/// The SID of the Medium mandatory level. The session of `user`'s commands is any but 0, which is the wrapper's own
/// rule (its session check), so that a user whose session is not the first is up too.
const MEDIUM: &str = "S-1-16-8192";

/// PowerShell 7. `whoami /groups` lists the token's mandatory label; its CSV header is localized, its `SID` column is
/// not.
const PROBE: &str = "$s = [Diagnostics.Process]::GetCurrentProcess().SessionId
$i = (whoami /groups /fo csv | ConvertFrom-Csv | Where-Object { $_.SID -like 'S-1-16-*' } | Select-Object -First 1).SID
\"DEVVM-READY session=$s integrity=$i\"
";

/// How one probe went.
enum ProbeOutcome {
    Ready,
    /// The probe ran, or was refused, where the user's commands do not run yet; the text says where.
    NotYet(String),
    /// The probe could not run: no retry will change that.
    Failed(Error),
}

/// Blocks until a command runs in the user's session at Medium integrity. Every other answer is asked again, paced by
/// the connection retry's pause; only a probe that cannot run is an error. A lost connection ends the call as
/// `connection-lost`: the engine's wait reconnects, since a guest may restart during its boot. No bound here: the
/// caller's boot bound ends the wait and names the last answer.
pub(super) fn wait(ssh: &SshSession) -> Result<Readiness> {
    let command = probe_command()?;
    let mut last = "nothing yet".to_owned();
    loop {
        let output = ssh.exec_collect(&command).map_err(|e| {
            let kind = e.kind();
            e.with_context(
                kind,
                format!("waiting for the user's session of the guest (not session 0, Medium integrity; the last answer: {last})"),
            )
        })?;
        match assess(&output) {
            ProbeOutcome::Ready => return Ok(Readiness::Ready),
            ProbeOutcome::NotYet(why) => last = why,
            ProbeOutcome::Failed(error) => return Err(error),
        }
        std::thread::sleep(crate::access::session::RETRY_PAUSE);
    }
}

/// The probe as the script sshd hands to ssh-broker.
fn probe_command() -> Result<String> {
    wrap(&Command::new([
        "pwsh",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        PROBE,
    ]))
}

fn assess(output: &Output) -> ProbeOutcome {
    let stdout = output.stdout_lossy();
    let stderr = output.stderr_lossy();
    let failed = |what: String| {
        ProbeOutcome::Failed(Error::new(
            ErrorKind::Io,
            format!(
                "the guest's readiness probe could not run: {what} (stdout: {}; stderr: {})",
                stdout.trim(),
                stderr.trim()
            ),
        ))
    };
    if let Some(loss) = &output.loss {
        return failed(format!("its output was lost ({loss})"));
    }
    match output.status {
        ExitStatus::Code(126) if stderr.contains(NOT_IN_SESSION) => ProbeOutcome::NotYet(NOT_IN_SESSION.to_owned()),
        ExitStatus::Code(0) => match answer(&stdout) {
            Some((session, integrity)) if session != 0 && integrity == MEDIUM => ProbeOutcome::Ready,
            Some((session, integrity)) => {
                ProbeOutcome::NotYet(format!("the session is {session} and the integrity level {integrity}"))
            }
            None => failed("its answer is not the probe's".to_owned()),
        },
        ExitStatus::Code(code) => failed(format!("it ended with {code}")),
        ExitStatus::Signal { ref name, .. } => failed(format!("it was killed by signal {name}")),
    }
}

/// The session and the integrity level the probe printed.
fn answer(stdout: &str) -> Option<(u32, String)> {
    let line = stdout.lines().find_map(|line| line.trim().strip_prefix(READY_PROBE))?;
    let mut words = line.split_whitespace();
    let session = words.next()?.strip_prefix("session=")?.parse().ok()?;
    let integrity = words.next()?.strip_prefix("integrity=")?;
    Some((session, integrity.to_owned()))
}
