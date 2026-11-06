use devvm::exit::ExitStatus;
use devvm::{Check, ErrorKind};

use crate::doctor::{doctor_to, line};

fn check(name: &str, outcome: Result<&str, &str>) -> Check {
    Check {
        name: name.to_owned(),
        outcome: outcome.map(str::to_owned).map_err(str::to_owned),
    }
}

fn printed(checks: Vec<Check>) -> (String, devvm::Result<ExitStatus>) {
    let mut out = Vec::new();
    let result = doctor_to(&mut out, checks);
    (String::from_utf8(out).unwrap(), result)
}

#[skuld::test]
fn a_line_says_pass_or_fail_then_the_check_and_what_was_found() {
    assert_eq!(line(&check("qemu", Ok("QEMU 11.1.2"))), "pass qemu: QEMU 11.1.2");
    assert_eq!(
        line(&check("git", Err("cannot run `git`"))),
        "fail git: cannot run `git`"
    );
}

#[skuld::test]
fn every_check_is_printed_and_all_passing_is_exit_0() {
    let (out, result) = printed(vec![
        check("accelerator", Ok("whpx")),
        check("git", Ok("git version 2")),
    ]);

    assert_eq!(out, "pass accelerator: whpx\npass git: git version 2\n");
    assert!(matches!(result, Ok(ExitStatus::Code(0))), "{result:?}");
}

#[skuld::test]
fn failures_are_all_printed_before_the_exit_says_how_many_checks_failed() {
    let (out, result) = printed(vec![
        check("accelerator", Err("no hypervisor")),
        check("git", Ok("git version 2")),
        check("qemu", Err("no build")),
    ]);

    assert_eq!(
        out,
        "fail accelerator: no hypervisor\npass git: git version 2\nfail qemu: no build\n"
    );
    let error = result.expect_err("two checks failed");
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert_eq!(error.message(), "2 check(s) failed");
}
