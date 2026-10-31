use super::readiness;
use crate::exit::{ExitStatus, Output};
use crate::guest::Readiness;
use crate::{Error, ErrorKind};

fn reply(code: i32, stdout: &str) -> Output {
    Output {
        status: ExitStatus::Code(code),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
        loss: None,
    }
}

#[skuld::test]
fn cloud_init_status_replies_map_to_ready_degraded_and_error() {
    let done =
        r#"{"boot_status_code": "enabled-by-generator", "errors": [], "extended_status": "done", "status": "done"}"#;
    assert_eq!(readiness(&reply(0, done)).unwrap(), Readiness::Ready);

    let degraded = r#"{"errors": [], "extended_status": "degraded done", "recoverable_errors": {"WARNING": ["x"]}, "status": "done"}"#;
    let Readiness::Degraded(what) = readiness(&reply(2, degraded)).unwrap() else {
        panic!("a degraded `done` is not plain readiness");
    };
    assert!(
        what.contains("degraded done") && what.contains("WARNING"),
        "the degradation is named: {what}"
    );

    let error = r#"{"errors": ["boom"], "extended_status": "error", "status": "error"}"#;
    let failed = readiness(&reply(1, error)).unwrap_err();
    assert_eq!(failed.kind(), ErrorKind::Io);
    assert!(
        failed.message().contains("boom"),
        "the reply is quoted: {}",
        failed.message()
    );

    let running = r#"{"extended_status": "running", "status": "running"}"#;
    let unfinished = readiness(&reply(0, running)).unwrap_err();
    assert!(unfinished.message().contains("running"), "{}", unfinished.message());
}

#[skuld::test]
fn an_unreadable_reply_is_an_error_carrying_the_output() {
    for (output, quoted) in [
        (reply(0, "not json at all"), "not json at all"),
        (reply(1, ""), "exit code 1"),
        (reply(0, r#"{"detail": "no status field"}"#), "no status field"),
        (reply(0, r#"{"status": 7}"#), r#"{"status": 7}"#),
        (reply(0, "[]"), "[]"),
    ] {
        let error = readiness(&output).unwrap_err();
        assert!(error.message().contains(quoted), "{quoted}: {}", error.message());
    }

    let mut with_stderr = reply(127, "");
    with_stderr.stderr = b"cloud-init: command not found".to_vec();
    let error = readiness(&with_stderr).unwrap_err();
    assert!(error.message().contains("command not found"), "{}", error.message());
}

#[skuld::test]
fn a_cloud_init_killed_by_a_signal_is_not_ready_whatever_it_printed() {
    let output = Output {
        status: ExitStatus::Signal {
            name: "KILL".to_owned(),
            core_dumped: false,
        },
        stdout: br#"{"status": "done"}"#.to_vec(),
        stderr: Vec::new(),
        loss: None,
    };
    let error = readiness(&output).unwrap_err();
    assert!(error.message().contains("KILL"), "{}", error.message());
}

#[skuld::test]
fn a_done_reply_is_trusted_only_with_cloud_inits_documented_exit_codes() {
    let done = r#"{"errors": [], "extended_status": "done", "status": "done"}"#;
    for code in [1, 3, 127, 255, -1] {
        let error = readiness(&reply(code, done)).expect_err(&format!("exit code {code} is not a normal finish"));
        assert_eq!(error.kind(), ErrorKind::Io);
        assert!(
            error.message().contains(&format!("exit code {code}")),
            "{code}: {}",
            error.message()
        );
        assert!(error.message().contains(r#""status": "done""#), "the reply is quoted");
    }

    // 2 is cloud-init's "recoverable errors", whatever else the reply says.
    let Readiness::Degraded(what) = readiness(&reply(2, done)).unwrap() else {
        panic!("exit code 2 is a degraded finish");
    };
    assert!(what.contains("recoverable"), "{what}");
}

#[skuld::test]
fn a_done_reply_with_errors_is_an_error_and_with_recoverable_errors_is_degraded() {
    let with_errors = r#"{"errors": ["boom"], "extended_status": "done", "status": "done"}"#;
    let error = readiness(&reply(0, with_errors)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains("boom"), "{}", error.message());

    let recoverable =
        r#"{"errors": [], "extended_status": "done", "recoverable_errors": {"WARNING": ["w"]}, "status": "done"}"#;
    let Readiness::Degraded(what) = readiness(&reply(0, recoverable)).unwrap() else {
        panic!("recoverable errors are a degraded finish even with exit code 0");
    };
    assert!(what.contains("WARNING"), "{what}");

    // Empty containers are not errors.
    let empty = r#"{"errors": [], "extended_status": "done", "recoverable_errors": {}, "status": "done"}"#;
    assert_eq!(readiness(&reply(0, empty)).unwrap(), Readiness::Ready);
}

#[skuld::test]
fn a_done_reply_with_a_degraded_extended_status_is_degraded_whatever_the_exit_code_says() {
    let degraded = r#"{"errors": [], "extended_status": "degraded done", "status": "done"}"#;
    let Readiness::Degraded(what) = readiness(&reply(0, degraded)).unwrap() else {
        panic!("a degraded extended status is a degraded finish");
    };
    assert!(what.contains("degraded done"), "{what}");
}

#[skuld::test]
fn a_status_other_than_done_is_an_error_naming_it() {
    for status in ["disabled", "not started"] {
        let text = format!(r#"{{"extended_status": "{status}", "status": "{status}"}}"#);
        let error = readiness(&reply(0, &text)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{status}");
        assert!(error.message().contains(status), "{status}: {}", error.message());
    }
}

#[skuld::test]
fn a_reply_cut_short_by_a_lost_connection_is_that_loss() {
    let output = Output {
        status: ExitStatus::Code(0),
        stdout: br#"{"extended_status": "do"#.to_vec(),
        stderr: Vec::new(),
        loss: Some(Error::new(ErrorKind::ConnectionLost, "the VM exited")),
    };
    let error = readiness(&output).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(
        error.message().contains("the VM exited"),
        "the loss is named: {}",
        error.message()
    );
    assert!(!error.message().contains("not JSON"), "{}", error.message());
}

/// The message up to the quoted command: what the error says is wrong.
fn headline(error: &Error) -> &str {
    error.message().split(" (`").next().unwrap()
}

#[skuld::test]
fn a_reply_that_is_not_json_is_blamed_on_the_exit_when_the_command_failed() {
    for (stdout, code) in [("", 127), ("usage: cloud-init ...", 2), ("oops", 1), ("", 126)] {
        let mut output = reply(code, stdout);
        output.stderr = b"bash: cloud-init: command not found\n".to_vec();
        let error = readiness(&output).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io);
        let head = headline(&error);
        assert!(head.contains(&format!("exit code {code}")), "{head}");
        assert!(head.contains("command not found"), "stderr is in the headline: {head}");
        assert!(!head.contains("not JSON"), "{head}");
    }

    // With no stderr the headline is still the exit.
    let head = headline(&readiness(&reply(1, "")).unwrap_err()).to_owned();
    assert!(head.contains("exit code 1"), "{head}");

    // Only an exit of 0 makes the unreadable reply the problem.
    let error = readiness(&reply(0, "not json at all")).unwrap_err();
    assert!(headline(&error).contains("not JSON"), "{}", error.message());
}

#[skuld::test]
fn a_cloud_init_killed_by_a_signal_has_the_signal_and_its_stderr_as_the_headline() {
    let output = Output {
        status: ExitStatus::Signal {
            name: "SEGV".to_owned(),
            core_dumped: true,
        },
        stdout: Vec::new(),
        stderr: b"Segmentation fault\n".to_vec(),
        loss: None,
    };
    let error = readiness(&output).unwrap_err();
    let head = headline(&error);
    assert!(
        head.contains("signal SEGV") && head.contains("Segmentation fault"),
        "{head}"
    );
}

#[skuld::test]
fn a_failed_exit_leads_even_when_the_connection_was_lost_afterwards() {
    let output = Output {
        status: ExitStatus::Code(127),
        stdout: Vec::new(),
        stderr: b"bash: cloud-init: command not found".to_vec(),
        loss: Some(Error::new(ErrorKind::ConnectionLost, "the VM exited")),
    };
    let error = readiness(&output).unwrap_err();
    let head = headline(&error);
    assert!(
        head.contains("exit code 127") && head.contains("command not found"),
        "{head}"
    );
    assert!(!head.contains("cut short"), "{head}");
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
}

#[skuld::test]
fn a_failed_exit_leads_a_reply_that_parses_but_cannot_be_read() {
    for code in [127, 1, 2] {
        for stdout in ["{}", "[]", "null", r#"{"status": 7}"#] {
            let mut output = reply(code, stdout);
            output.stderr = b"bash: cloud-init: command not found
"
            .to_vec();
            let error = readiness(&output).unwrap_err();
            let head = headline(&error);
            assert!(
                head.contains(&format!("exit code {code}")) && head.contains("command not found"),
                "{code} {stdout}: {head}"
            );
            assert!(!head.contains("`status`"), "{code} {stdout}: {head}");
        }
    }

    // Exit 0 makes the unreadable reply the problem.
    for stdout in ["{}", "[]", "null"] {
        let error = readiness(&reply(0, stdout)).unwrap_err();
        assert!(headline(&error).contains("`status`"), "{stdout}: {}", error.message());
    }
}

#[skuld::test]
fn stderr_appears_once_in_every_readiness_error() {
    const MARK: &str = "STDERR-MARK";
    let signal = ExitStatus::Signal {
        name: "SEGV".to_owned(),
        core_dumped: false,
    };
    let loss = || Some(Error::new(ErrorKind::ConnectionLost, "the VM exited"));
    let cases = [
        (ExitStatus::Code(127), "", None),
        (ExitStatus::Code(127), "{}", None),
        (ExitStatus::Code(127), "", loss()),
        (ExitStatus::Code(0), "not json", None),
        (ExitStatus::Code(0), "{}", None),
        (ExitStatus::Code(0), r#"{"status": "running"}"#, None),
        (ExitStatus::Code(0), r#"{"status": "done", "errors": ["boom"]}"#, None),
        (ExitStatus::Code(1), r#"{"status": "done"}"#, None),
        (ExitStatus::Code(0), r#"{"extended_status": "do"#, loss()),
        (signal, "", None),
    ];
    for (status, stdout, loss) in cases {
        let output = Output {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: MARK.as_bytes().to_vec(),
            loss,
        };
        let error = readiness(&output).unwrap_err();
        assert_eq!(
            error.message().matches(MARK).count(),
            1,
            "{output:?}: {}",
            error.message()
        );
    }
}
