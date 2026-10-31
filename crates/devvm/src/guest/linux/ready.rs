//! Telling from `cloud-init status` whether a first boot has finished.

use crate::exit::{ExitStatus, Output};
use crate::guest::Readiness;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "ready_tests.rs"]
mod ready_tests;

/// Blocks until cloud-init has finished, and prints its state as JSON.
pub(super) const CLOUD_INIT_STATUS: &str = "cloud-init status --wait --format json";

/// Reads the reply of [`CLOUD_INIT_STATUS`]. Anything but a `done` that agrees with its exit code is an error that
/// quotes the reply. A command that failed is reported by its exit, before anything about its reply. The reply of a
/// command that exited 0 and cannot be read because `output.loss` cut it short is an error of the loss's kind.
pub(super) fn readiness(output: &Output) -> Result<Readiness> {
    let failure = |what: &str| {
        Error::new(
            ErrorKind::Io,
            format!(
                "cloud-init did not finish a normal first boot: {what} (`{CLOUD_INIT_STATUS}`: {})",
                describe(output, true)
            ),
        )
    };
    let code = match &output.status {
        ExitStatus::Code(code) => *code,
        ExitStatus::Signal { name, .. } => {
            return Err(command_failure(output, &format!("was killed by signal {name}")));
        }
    };
    let parsed = serde_json::from_slice::<serde_json::Value>(&output.stdout);
    let reply = parsed
        .as_ref()
        .ok()
        .filter(|reply| reply.get("status").is_some_and(serde_json::Value::is_string));
    let Some(reply) = reply else {
        // A failed command's own exit is the cause of whatever is wrong with its reply, and of a connection lost after
        // the exit arrived.
        if code != 0 {
            return Err(command_failure(output, &format!("exited with exit code {code}")));
        }
        return Err(match (&parsed, &output.loss) {
            // The connection broke after the status arrived: the reply is cut short, and the loss is the cause.
            (Err(_), Some(loss)) => Error::new(
                loss.kind(),
                format!(
                    "cloud-init's reply was cut short: {} (`{CLOUD_INIT_STATUS}`: {})",
                    loss.message(),
                    describe(output, true)
                ),
            ),
            (Err(e), None) => failure(&format!("its reply is not JSON ({e})")),
            (Ok(_), _) => failure("its reply has no `status` text"),
        });
    };
    match reply["status"].as_str().unwrap_or_default() {
        "done" => done(reply, code, &failure),
        other => Err(failure(&format!("its status is `{other}`"))),
    }
}

/// The error for a `cloud-init status` that did not exit normally: what happened to it, then its stderr, which is
/// then left out of the quoted output.
fn command_failure(output: &Output, what: &str) -> Error {
    let stderr = match output.stderr_lossy().trim() {
        "" => String::new(),
        stderr => format!(": {stderr}"),
    };
    Error::new(
        ErrorKind::Io,
        format!(
            "cloud-init did not finish a normal first boot: it {what}{stderr} (`{CLOUD_INIT_STATUS}`: {})",
            describe(output, false)
        ),
    )
}

/// A `done` reply, checked against the exit code and the error lists. cloud-init exits 0 for a clean finish and 2 when
/// it recorded recoverable errors; any other code, or a non-empty `errors`, contradicts `done`.
fn done(reply: &serde_json::Value, code: i32, failure: &dyn Fn(&str) -> Error) -> Result<Readiness> {
    let nonempty = |key: &str| {
        reply.get(key).filter(|value| match value {
            serde_json::Value::Null => false,
            serde_json::Value::Array(items) => !items.is_empty(),
            serde_json::Value::Object(fields) => !fields.is_empty(),
            _ => true,
        })
    };
    if let Some(errors) = nonempty("errors") {
        return Err(failure(&format!(
            "its status is `done` but it reports errors: {errors}"
        )));
    }
    if !matches!(code, 0 | 2) {
        return Err(failure(&format!(
            "its status is `done` but it exited with exit code {code}, not 0 or 2"
        )));
    }
    let degraded_text = reply
        .get("extended_status")
        .and_then(serde_json::Value::as_str)
        .filter(|text| text.starts_with("degraded"));
    let recoverable = nonempty("recoverable_errors");
    if code != 2 && degraded_text.is_none() && recoverable.is_none() {
        return Ok(Readiness::Ready);
    }
    let head = degraded_text.unwrap_or("recoverable errors");
    Ok(Readiness::Degraded(match recoverable {
        Some(errors) => format!("{head}: {errors}"),
        None => head.to_owned(),
    }))
}

/// The exit code and stdout of `output`, and its stderr when `stderr` is set, for an error message.
fn describe(output: &Output, stderr: bool) -> String {
    let code = match &output.status {
        ExitStatus::Code(code) => format!("exit code {code}"),
        ExitStatus::Signal { name, .. } => format!("signal {name}"),
    };
    let mut text = format!("{code}; stdout: {}", output.stdout_lossy().trim());
    if stderr {
        text.push_str(&format!("; stderr: {}", output.stderr_lossy().trim()));
    }
    text
}
