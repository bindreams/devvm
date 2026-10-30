//! Telling from `cloud-init status` whether a first boot has finished.

use crate::exit::{ExitStatus, Output};
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "ready_tests.rs"]
mod ready_tests;

/// Blocks until cloud-init has finished, and prints its state as JSON.
pub(super) const CLOUD_INIT_STATUS: &str = "cloud-init status --wait --format json";

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Readiness {
    Ready,
    /// cloud-init finished with recoverable errors (it exits 2); the guest is usable.
    Degraded,
}

/// Reads the reply of [`CLOUD_INIT_STATUS`]. Anything but `done` is an error that quotes the reply.
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
    if let ExitStatus::Signal { name, .. } = &output.status {
        return Err(failure(&format!("it was killed by signal {name}")));
    }
    let reply: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| failure(&format!("its reply is not JSON ({e})")))?;
    let Some(status) = reply.get("status").and_then(serde_json::Value::as_str) else {
        return Err(failure("its reply has no `status` text"));
    };
    match status {
        "done" => {
            let extended = reply.get("extended_status").and_then(serde_json::Value::as_str);
            Ok(if extended.is_some_and(|text| text.starts_with("degraded")) {
                Readiness::Degraded
            } else {
                Readiness::Ready
            })
        }
        other => Err(failure(&format!("its status is `{other}`"))),
    }
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
