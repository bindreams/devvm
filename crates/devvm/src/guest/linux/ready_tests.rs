use super::{CLOUD_INIT_STATUS, readiness};
use crate::ErrorKind;
use crate::exit::{ExitStatus, Output};
use crate::guest::Readiness;

fn reply(code: i32, stdout: &str) -> Output {
    Output {
        status: ExitStatus::Code(code),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
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
    };
    let error = readiness(&output).unwrap_err();
    assert!(error.message().contains("KILL"), "{}", error.message());
}

#[skuld::test]
fn the_status_command_waits_and_asks_for_json() {
    assert_eq!(CLOUD_INIT_STATUS, "cloud-init status --wait --format json");
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
