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
/// quotes the reply. The error is built from every fact in `output` (see [`Cause`] for which one leads) and keeps all of
/// them.
pub(super) fn readiness(output: &Output) -> Result<Readiness> {
    assess(output).map_err(|cause| cause.into_error(output))
}

/// Why `cloud-init status` is not a normal finish, as the headline of the error. The first that holds is the cause:
/// a signal; an exit code cloud-init does not document (not 0, 1 or 2); a loss of the output (see [`Output::loss`]),
/// when the exit is documented and the reply cannot be read; the reply's own problem. For a documented exit with a
/// readable reply the reply's text is the cause.
enum Cause<'a> {
    Signal(&'a str),
    UndocumentedExit(i32),
    OutputLost(&'a Error),
    /// A `done` reply with exit code 1.
    DoneWithExit1,
    Reply(String),
}

fn assess(output: &Output) -> std::result::Result<Readiness, Cause<'_>> {
    let code = match &output.status {
        ExitStatus::Code(code) => *code,
        ExitStatus::Signal { name, .. } => return Err(Cause::Signal(name)),
    };
    if !matches!(code, 0..=2) {
        return Err(Cause::UndocumentedExit(code));
    }
    let parsed = serde_json::from_slice::<serde_json::Value>(&output.stdout);
    let reply = parsed
        .as_ref()
        .ok()
        .filter(|reply| reply.get("status").is_some_and(serde_json::Value::is_string));
    let Some(reply) = reply else {
        return Err(match (&output.loss, &parsed) {
            (Some(loss), _) => Cause::OutputLost(loss),
            (None, Err(e)) => Cause::Reply(format!("its reply is not JSON ({e})")),
            (None, Ok(_)) => Cause::Reply("its reply has no `status` text".to_owned()),
        });
    };
    match reply["status"].as_str().unwrap_or_default() {
        "done" => done(reply, code),
        other => Err(Cause::Reply(format!("its status is `{other}`"))),
    }
}

impl Cause<'_> {
    /// The error: the cause as the headline, then the facts it does not already carry (the exit or signal, whether it
    /// dumped core, stdout, stderr, the loss). The kind is the loss's when the output was lost, `Io` otherwise.
    fn into_error(self, output: &Output) -> Error {
        let stderr = output.stderr_lossy();
        let stderr = stderr.trim();
        // A command that did not exit normally says why in its stderr, so the headline carries it.
        let with_stderr = |what: String| {
            if stderr.is_empty() {
                what
            } else {
                format!("{what}: {stderr}")
            }
        };
        let (kind, headline) = match &self {
            Cause::Signal(name) => (ErrorKind::Io, with_stderr(format!("it was killed by signal {name}"))),
            Cause::UndocumentedExit(code) => (ErrorKind::Io, with_stderr(format!("it exited with exit code {code}"))),
            Cause::OutputLost(loss) => (loss.kind(), format!("the output was lost ({loss})")),
            Cause::DoneWithExit1 => (
                ErrorKind::Io,
                "its status is `done` but its exit code 1 is not 0 or 2".to_owned(),
            ),
            Cause::Reply(problem) => (ErrorKind::Io, problem.clone()),
        };
        let stderr_led = matches!(self, Cause::Signal(_) | Cause::UndocumentedExit(_)) && !stderr.is_empty();
        let loss_led = matches!(self, Cause::OutputLost(_));
        let exit_led = matches!(
            self,
            Cause::Signal(_) | Cause::UndocumentedExit(_) | Cause::DoneWithExit1
        );
        let mut facts = Vec::new();
        // A signal always leads (see `assess`), so only whether it dumped core is left for the tail.
        match &output.status {
            ExitStatus::Code(code) if !exit_led => facts.push(format!("exit code {code}")),
            ExitStatus::Signal { core_dumped: true, .. } => facts.push("core dumped".to_owned()),
            _ => {}
        }
        let stdout = output.stdout_lossy();
        let stdout = stdout.trim();
        if !stdout.is_empty() {
            facts.push(format!("stdout: {stdout}"));
        }
        if !stderr_led && !stderr.is_empty() {
            facts.push(format!("stderr: {stderr}"));
        }
        if let (Some(loss), false) = (&output.loss, loss_led) {
            facts.push(loss.to_string());
        }
        let facts = if facts.is_empty() {
            String::new()
        } else {
            format!(": {}", facts.join("; "))
        };
        Error::new(
            kind,
            format!("cloud-init did not finish a normal first boot: {headline} (`{CLOUD_INIT_STATUS}`{facts})"),
        )
    }
}

/// A `done` reply, checked against the exit code (0, 1 or 2 here) and the error lists. cloud-init exits 0 for a clean
/// finish and 2 when it recorded recoverable errors; 1, or a non-empty `errors`, contradicts `done`.
fn done(reply: &serde_json::Value, code: i32) -> std::result::Result<Readiness, Cause<'static>> {
    let nonempty = |key: &str| {
        reply.get(key).filter(|value| match value {
            serde_json::Value::Null => false,
            serde_json::Value::Array(items) => !items.is_empty(),
            serde_json::Value::Object(fields) => !fields.is_empty(),
            _ => true,
        })
    };
    if let Some(errors) = nonempty("errors") {
        return Err(Cause::Reply(format!(
            "its status is `done` but it reports errors: {errors}"
        )));
    }
    if !matches!(code, 0 | 2) {
        debug_assert_eq!(
            code, 1,
            "the other undocumented codes are caught before the reply is read"
        );
        return Err(Cause::DoneWithExit1);
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
