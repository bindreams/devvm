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
/// quotes the reply.
pub(super) fn readiness(output: &Output) -> Result<Readiness> {
    let failure = |what: &str| {
        Error::new(
            ErrorKind::Io,
            format!(
                "cloud-init did not finish a normal first boot: {what} (`{CLOUD_INIT_STATUS}`: {})",
                describe(output)
            ),
        )
    };
    let code = match &output.status {
        ExitStatus::Code(code) => *code,
        ExitStatus::Signal { name, .. } => return Err(failure(&format!("it was killed by signal {name}"))),
    };
    let reply: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| failure(&format!("its reply is not JSON ({e})")))?;
    let Some(status) = reply.get("status").and_then(serde_json::Value::as_str) else {
        return Err(failure("its reply has no `status` text"));
    };
    match status {
        "done" => done(&reply, code, &failure),
        other => Err(failure(&format!("its status is `{other}`"))),
    }
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

/// The exit code, stdout and stderr of `output`, for an error message.
fn describe(output: &Output) -> String {
    let code = match &output.status {
        ExitStatus::Code(code) => format!("exit code {code}"),
        ExitStatus::Signal { name, .. } => format!("signal {name}"),
    };
    format!(
        "{code}; stdout: {}; stderr: {}",
        output.stdout_lossy().trim(),
        output.stderr_lossy().trim()
    )
}
