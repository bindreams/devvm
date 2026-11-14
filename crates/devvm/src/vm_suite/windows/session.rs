//! Which session and integrity level a command runs in: the user's session 1 at Medium, never session 0, High only when
//! asked for or when the guest's policy approves it.

use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use super::bounds::{BOOT_BOUND, STATE_BOUND};
use super::{FACTS, PACE, SharedWindowsGuest, WindowsBase, facts, printed, pwsh, ran, user_facts, windows_options};
use super::{shared_windows_guest, windows_base, windows_iso, windows_recipe};
use crate::guest::GuestOs;
use crate::guest::windows::WindowsGuest;
use crate::host;
use crate::vm_suite::fixture::Up;
use crate::vm_suite::waits::{ANSWER_BOUND, awaiting, step};
use crate::{BuildOptions, Command, ErrorKind, ExitStatus, Guest, Output, RunAs};

#[skuld::test]
fn windows_run_is_medium_integrity_in_session_1(#[fixture(shared_windows_guest)] guest: &SharedWindowsGuest) {
    assert_eq!(facts(&guest.vm, RunAs::User), user_facts());
}

#[skuld::test]
fn windows_elevated_run_is_high_integrity_without_uac_auto_approve(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    // The policy that makes an administrator's elevation silent is not set in this base: High is what `--elevated`
    // asked for, through the helper task.
    let policy = printed(
        &guest.vm,
        r"(Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System').ConsentPromptBehaviorAdmin",
    );
    assert_ne!(policy, "0", "this base auto-approves elevation");

    assert_eq!(
        facts(&guest.vm, RunAs::Elevated),
        super::elevated_facts(),
        "an elevated command runs at High in session 1"
    );
}

// The agent ===========================================================================================================

/// Ends the agent of ssh-broker in the user's session: the process `ssh-broker.exe agent`, and not the `ssh-broker.exe
/// -c ...` that sshd runs for each command.
const STOP_AGENT: &str = r#"Get-CimInstance Win32_Process -Filter "Name = 'ssh-broker.exe'" | Where-Object { $_.CommandLine -match 'ssh-broker\.exe"?\s+agent(\s|$)' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }"#;

/// Runs `command` until the connection to the guest holds: the one that served the command before it is still closing.
/// A connection that does not hold within [`STATE_BOUND`] fails the test with the loss; any other error fails it at once.
fn answered(guest: &dyn Guest, what: &str, command: &Command) -> Output {
    let deadline = Instant::now() + STATE_BOUND;
    loop {
        match ran(guest, what, command) {
            Ok(output) => return output,
            Err(e) if e.kind() == ErrorKind::ConnectionLost => {
                assert!(
                    Instant::now() < deadline,
                    "cannot run {:?}: the connection did not hold within {STATE_BOUND:?}: {e}",
                    command.argv
                );
                std::thread::sleep(PACE);
            }
            Err(e) => panic!("cannot run {:?}: {e}", command.argv),
        }
    }
}

#[skuld::test]
fn windows_a_command_after_the_agent_is_stopped_fails_in_the_preamble_instead_of_running_in_session_0(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    step("booting an instance");
    let up = Up::new(&base.image, "w-agent-stopped", &windows_options()).unwrap();
    assert_eq!(facts(&*up, RunAs::User), user_facts());

    step("stopping the agent");
    // The agent that serves this command ends with it: the connection is lost, or the command ends with a status that
    // is not its own. Either is the end of the agent.
    let _ = ran(&*up, "the command that stops the agent", &pwsh(RunAs::User, STOP_AGENT));

    step("running a command after the agent stopped");
    let seen = answered(&*up, "whoami after the agent stopped", &Command::new(["whoami"]));
    // The documented limit: the command runs nowhere, and says so. 126 is a guest status like any other.
    assert_eq!(seen.status, ExitStatus::Code(126), "{}", seen.stderr_lossy());
    assert_eq!(seen.stdout_lossy(), "", "a command printed output in session 0");
    assert!(
        seen.stderr_lossy()
            .contains("devvm: the command did not reach the user's session"),
        "{}",
        seen.stderr_lossy()
    );
}

// The boot window =====================================================================================================

/// What a plain `ssh` of the generated `config` gets from `name` for the command `remote`.
pub(super) fn ssh_remote(config: &Path, name: &str, remote: &str) -> std::process::Output {
    let _watch = awaiting(format!("ssh to {name}"), ANSWER_BOUND);
    let mut command = std::process::Command::new("ssh");
    command
        .arg("-F")
        .arg(config)
        .args(["-o", "BatchMode=yes"])
        // A connection that the guest does not answer yet is no answer: the next probe asks again.
        .args(["-o", "ConnectTimeout=10"])
        .arg(name)
        .arg(remote)
        .stdin(Stdio::null());
    host::current().quiet(&mut command);
    command.output().unwrap_or_else(|e| panic!("cannot run ssh: {e}"))
}

/// What a probe through `ssh` says: the `session=.. integrity=..` line of a command that ran, or nothing.
fn answer(output: &std::process::Output) -> Option<String> {
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .map(str::trim)
        .find(|line| line.starts_with("session="))
        .map(str::to_owned)
}

/// The two ways a command reaches the guest through `ssh`: a raw one, which a login or `sftp` is like (no preamble), and
/// one wrapped as devvm wraps its own.
fn probes() -> [(&'static str, String); 2] {
    let wrapped = Command::new(["pwsh", "-NoProfile", "-NonInteractive", "-Command", FACTS]);
    [
        ("a raw command", FACTS.to_owned()),
        ("a wrapped command", WindowsGuest.wrap(&wrapped).unwrap()),
    ]
}

/// Starts an instance while probing it through `ssh` from the moment it is launched, with raw commands and wrapped ones.
/// A probe the guest answers must be answered in session 1 at Medium, or the test fails; the probing ends at the first
/// answer. The window is observed, not proven closed: the proof is the order of the boot task's steps (sshd starts only
/// once the agent accepts a connection), which the template tests pin.
#[skuld::test]
fn windows_no_command_is_answered_from_session_0_while_booting(#[fixture(windows_base)] base: &WindowsBase) {
    let _watch = awaiting("the instance starting while probed", BOOT_BOUND);
    let up = Up::created(&base.image, "w-boot-window", &windows_options()).unwrap();
    let config = base.engine.ssh_configs().config();
    let name = up.name().to_owned();

    std::thread::scope(|scope| {
        let start = scope.spawn(|| up.start());
        'probing: while !start.is_finished() {
            for (kind, remote) in probes() {
                if let Some(line) = answer(&ssh_remote(&config, &name, &remote)) {
                    assert_eq!(line, user_facts(), "{kind} was answered while the guest booted");
                    break 'probing;
                }
            }
            std::thread::sleep(PACE);
        }
        start.join().unwrap().unwrap();
    });

    step("probing the started instance");
    // The probes themselves work: once the guest is up, each is answered in session 1 at Medium.
    for (kind, remote) in probes() {
        let output = ssh_remote(&config, &name, &remote);
        assert_eq!(
            answer(&output).as_deref(),
            Some(user_facts().as_str()),
            "{kind} after the start: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

// UAC =================================================================================================================

/// A child that is started with `runas`: it records where it runs and ends with status 7. `Start-Process -Verb RunAs`
/// waits on a consent prompt that nobody answers unless the guest's policy approves it.
fn runas_script() -> String {
    format!(
        r#"$out = 'C:\ProgramData\devvm\runas-facts.txt'
Remove-Item -LiteralPath $out -ErrorAction Ignore
$child = @'
& {{ {FACTS} }} | Set-Content -LiteralPath 'C:\ProgramData\devvm\runas-facts.txt'; exit 7
'@
$encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($child))
$process = Start-Process -FilePath pwsh -Verb RunAs -Wait -PassThru -ArgumentList '-NoProfile', '-NonInteractive', '-EncodedCommand', $encoded
"exit=$($process.ExitCode)"
Get-Content -LiteralPath $out"#
    )
}

#[skuld::test]
fn windows_uac_auto_approve_lets_a_runas_child_reach_high_integrity_without_a_prompt(
    #[fixture(windows_base)] base: &WindowsBase,
    #[fixture(windows_iso)] iso: &Path,
) {
    step("building the base with the policy set");
    // One step boots a guest of the base and sets the policy, elevated.
    let recipe = windows_recipe(&base.roots.dirs.state, iso, &["set: { uac: auto-approve }"]);
    let image = base.engine.build(&recipe, &BuildOptions::default()).unwrap();
    step("booting an instance of it");
    let up = Up::new(&image, "w-uac", &windows_options()).unwrap();

    step("running the runas child");
    let printed = printed(&*up, &runas_script());

    let lines: Vec<&str> = printed.lines().map(str::trim).collect();
    assert_eq!(lines, ["exit=7", super::elevated_facts().as_str()], "{printed}");
}
