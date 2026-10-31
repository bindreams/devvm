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

// The error as one structure ==========================================================================================

/// What the headline of a readiness error says went wrong, in the order the causes rank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cause {
    Signal,
    UndocumentedExit,
    LostConnection,
    NotJson,
    NoStatus,
    NotDone,
    DoneWithErrors,
    DoneWithExit1,
}

impl Cause {
    const ALL: [Cause; 8] = [
        Cause::Signal,
        Cause::UndocumentedExit,
        Cause::LostConnection,
        Cause::NotJson,
        Cause::NoStatus,
        Cause::NotDone,
        Cause::DoneWithErrors,
        Cause::DoneWithExit1,
    ];

    /// Text only this cause's headline contains.
    fn marker(self) -> &'static str {
        match self {
            Cause::Signal => "killed by signal",
            Cause::UndocumentedExit => "exited with exit code",
            Cause::LostConnection => "connection was lost",
            Cause::NotJson => "not JSON",
            Cause::NoStatus => "no `status` text",
            Cause::NotDone => "its status is `running`",
            Cause::DoneWithErrors => "reports errors",
            Cause::DoneWithExit1 => "not 0 or 2",
        }
    }
}

/// The message up to the quoted command: what the error says is wrong.
fn headline(error: &Error) -> &str {
    error.message().split(" (`").next().unwrap()
}

#[skuld::test]
fn every_readiness_error_leads_with_the_first_true_cause_and_keeps_every_fact() {
    const MARK: &str = "STDERR-MARK";
    const LOSS: &str = "the VM exited";
    let statuses = [
        ("exit code 0", ExitStatus::Code(0)),
        ("exit code 1", ExitStatus::Code(1)),
        ("exit code 2", ExitStatus::Code(2)),
        ("exit code 127", ExitStatus::Code(127)),
        (
            "signal SEGV",
            ExitStatus::Signal {
                name: "SEGV".to_owned(),
                core_dumped: false,
            },
        ),
        (
            "signal SEGV",
            ExitStatus::Signal {
                name: "SEGV".to_owned(),
                core_dumped: true,
            },
        ),
    ];
    // The stdout, and the cause of an unusable reply with / without a loss.
    let replies = [
        (r#"{"status": "done", "errors": []}"#, None),
        (r#"{"status": "done", "errors": ["boom"]}"#, Some(Cause::DoneWithErrors)),
        (r#"{"status": "running"}"#, Some(Cause::NotDone)),
        ("{}", Some(Cause::NoStatus)),
        ("not json at all", Some(Cause::NotJson)),
        ("", Some(Cause::NotJson)),
    ];
    for (fact, status) in &statuses {
        for lost in [false, true] {
            for (stdout, reply_cause) in &replies {
                let output = Output {
                    status: status.clone(),
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: MARK.as_bytes().to_vec(),
                    loss: lost.then(|| Error::new(ErrorKind::ConnectionLost, LOSS)),
                };
                let unusable = matches!(reply_cause, Some(Cause::NotJson | Cause::NoStatus));
                let expected = match status {
                    ExitStatus::Signal { .. } => Some(Cause::Signal),
                    ExitStatus::Code(code) if !matches!(code, 0..=2) => Some(Cause::UndocumentedExit),
                    ExitStatus::Code(_) if unusable && lost => Some(Cause::LostConnection),
                    ExitStatus::Code(1) if reply_cause.is_none() => Some(Cause::DoneWithExit1),
                    ExitStatus::Code(_) => *reply_cause,
                };
                let row = format!("{status:?} lost={lost} stdout={stdout:?}");
                let result = readiness(&output);
                let Some(expected) = expected else {
                    assert!(result.is_ok(), "{row}: {result:?}");
                    continue;
                };
                let error = result.expect_err(&row);
                let message = error.message();
                let head = headline(&error);

                // The headline is the expected cause and no other.
                for cause in Cause::ALL {
                    assert_eq!(
                        head.contains(cause.marker()),
                        cause == expected,
                        "{row}: {cause:?}: {head}"
                    );
                }
                let kind = if expected == Cause::LostConnection {
                    ErrorKind::ConnectionLost
                } else {
                    ErrorKind::Io
                };
                assert_eq!(error.kind(), kind, "{row}: {message}");

                // Every fact given is in the message.
                assert!(message.contains(fact), "{row}: the exit: {message}");
                let core = matches!(status, ExitStatus::Signal { core_dumped: true, .. });
                assert_eq!(message.contains("core dumped"), core, "{row}: core_dumped: {message}");
                if lost {
                    let loss = Error::new(ErrorKind::ConnectionLost, LOSS).to_string();
                    assert!(message.contains(&loss), "{row}: the loss and its kind: {message}");
                }
                if !stdout.is_empty() {
                    assert!(message.contains(stdout), "{row}: stdout: {message}");
                }
                assert_eq!(message.matches(MARK).count(), 1, "{row}: stderr once: {message}");
            }
        }
    }
}
