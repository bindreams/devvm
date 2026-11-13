//! The readiness probe in a real PowerShell 7 (the `pwsh_wrap` group): the one part of the readiness that runs on the
//! guest, whose output `assess` reads. The other tests answer it with text written by hand.

use super::{MEDIUM, ProbeOutcome, answer, assess, probe_command};
use crate::exit::{ExitStatus, Output};
use crate::guest::windows::wrap::wrap_pwsh_tests::{run, shown};
use crate::test_groups::{Group, pwsh_wrap};

/// What a PowerShell prints for `script`, trimmed.
fn printed(script: &str) -> String {
    let output = run(script, "none", None);
    assert_eq!(output.status.code(), Some(0), "{script}: {}", shown(&output));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[skuld::test]
fn the_probe_prints_the_session_and_the_integrity_that_assess_reads(#[fixture(pwsh_wrap)] _group: &Group) {
    let finished = run(&probe_command().unwrap(), "none", None);
    assert_eq!(finished.status.code(), Some(0), "{}", shown(&finished));
    let stdout = String::from_utf8_lossy(&finished.stdout).into_owned();
    let (session, integrity) = answer(&stdout).unwrap_or_else(|| panic!("{}", shown(&finished)));

    // Both values are asked another way: the session of a PowerShell of its own, and the integrity from the token's
    // elevation (an elevated token of an administrator is High, any other Medium).
    assert_eq!(
        session.to_string(),
        printed("[Diagnostics.Process]::GetCurrentProcess().SessionId")
    );
    let elevated = printed(
        "[Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)",
    );
    let derived = if elevated == "True" { "S-1-16-12288" } else { MEDIUM };
    assert_eq!(integrity, derived, "{}", shown(&finished));

    // What `assess` makes of it: ready only at Medium.
    let output = Output {
        status: ExitStatus::Code(0),
        stdout: finished.stdout,
        stderr: finished.stderr,
        loss: None,
    };
    match assess(&output) {
        ProbeOutcome::Ready => assert_eq!(integrity, MEDIUM),
        ProbeOutcome::NotYet(why) => {
            assert_ne!(integrity, MEDIUM, "{why}");
            assert!(why.contains(&integrity), "{why}");
        }
        ProbeOutcome::Failed(error) => panic!("{error}"),
    }
}
