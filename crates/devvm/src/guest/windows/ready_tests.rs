use std::path::Path;
use std::time::Duration;

use crate::ErrorKind;
use crate::access::{BootBound, VmExit};
use crate::guest::windows::WindowsGuest;
use crate::guest::{GuestOs, Readiness};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::sshd::{ProbeReply, Rig};

const NOT_IN_SESSION: &str = "devvm: the command did not reach the user's session\n";

fn session_0() -> ProbeReply {
    ProbeReply::new(126, "", NOT_IN_SESSION)
}

fn medium() -> ProbeReply {
    ProbeReply::new(0, "DEVVM-READY session=1 integrity=S-1-16-8192\r\n", "")
}

fn rig(home: &Path, probes: Vec<ProbeReply>) -> Rig {
    Rig::with(home, |options| options.ready_probes = probes)
}

#[skuld::test]
fn a_probe_from_session_0_is_retried(#[fixture(unicode_home)] home: &Path) {
    let rig = rig(home, vec![session_0(), session_0(), medium()]);
    let readiness = WindowsGuest.wait_ready(&rig.connect()).unwrap();
    assert_eq!(readiness, Readiness::Ready);
    assert_eq!(rig.sshd.commands().len(), 3);
}

#[skuld::test]
fn a_probe_from_session_1_medium_is_ready(#[fixture(unicode_home)] home: &Path) {
    let rig = rig(home, vec![medium()]);
    assert_eq!(WindowsGuest.wait_ready(&rig.connect()).unwrap(), Readiness::Ready);
    assert_eq!(rig.sshd.commands().len(), 1);
}

#[skuld::test]
fn a_probe_that_cannot_run_is_an_error_naming_the_cause(#[fixture(unicode_home)] home: &Path) {
    for (reply, cause) in [
        (
            ProbeReply::new(127, "", "devvm: pwsh: command not found\n"),
            "pwsh: command not found",
        ),
        (ProbeReply::new(1, "", "Access denied\n"), "Access denied"),
        (ProbeReply::new(0, "something else\n", ""), "something else"),
        (ProbeReply::new(0, "DEVVM-READY session=x\n", ""), "session=x"),
    ] {
        let rig = rig(home, vec![reply.clone()]);
        let error = WindowsGuest.wait_ready(&rig.connect()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{cause}: {error}");
        assert!(error.message().contains(cause), "{cause}: {error}");
        // It is not retried: nobody would know why the guest never became ready.
        assert_eq!(rig.sshd.commands().len(), 1, "{cause}");
    }
}

/// The boot bound is the one failure bound of the wait, which re-checks its condition until it holds.
fn the_bound_ends_the_wait(home: &Path, probes: Vec<ProbeReply>) -> crate::Error {
    let rig = rig(home, probes);
    let bound = BootBound::start(Duration::from_secs(2)).unwrap();
    let session = rig.connect_with(VmExit::never(), Some(bound)).unwrap();
    let error = WindowsGuest.wait_ready(&session).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("session 1"), "{error}");
    assert!(
        error.message().contains(&rig.serial_log.display().to_string()),
        "{error}"
    );
    error
}

#[skuld::test]
fn ready_is_never_reported_from_a_plain_session(#[fixture(unicode_home)] home: &Path) {
    let error = the_bound_ends_the_wait(home, vec![session_0()]);
    assert!(error.message().contains("did not reach the user's session"), "{error}");
}

#[skuld::test]
fn a_probe_from_session_1_at_high_is_not_ready(#[fixture(unicode_home)] home: &Path) {
    let high = ProbeReply::new(0, "DEVVM-READY session=1 integrity=S-1-16-12288\n", "");
    let error = the_bound_ends_the_wait(home, vec![high]);
    assert!(error.message().contains("S-1-16-12288"), "{error}");
}
