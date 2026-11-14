//! The license read of a Windows base, and when it ends.

use std::fmt;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "expiry_tests.rs"]
mod expiry_tests;

/// The first-logon script's marker: `DEVVM-LICENSE=<ASCII JSON>`.
const MARKER: &str = "DEVVM-LICENSE=";

/// `SoftwareLicensingProduct`'s `LicenseStatus` for a licensed product.
const LICENSED: u32 = 1;

/// The fields of the read that are numbers, and the guest's account of its activation; the description and the family are
/// only echoed in logs, so no localized text is read except the activation's message, which is echoed too.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct License {
    license_status: u32,
    /// Minutes until a time-based license ends.
    grace_period_remaining: u64,
    /// What the guest's call of `Activate` came to. Absent or `null` when none was made.
    activation: Option<Activation>,
}

/// What the first-logon script says of its call of `Activate`: text for a message, never read as meaning.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Activation {
    /// `succeeded` or `failed`.
    outcome: String,
    /// The failure's message, trimmed.
    message: Option<String>,
    /// The CIM exception's native error code and status code, when it has them.
    native_error_code: Option<String>,
    status_code: Option<i64>,
}

impl fmt::Display for Activation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.outcome)?;
        if let Some(message) = &self.message {
            write!(f, ": {message}")?;
        }
        let codes: Vec<String> = [
            self.native_error_code
                .as_ref()
                .map(|code| format!("native error code {code}")),
            self.status_code.map(|code| format!("status code {code}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !codes.is_empty() {
            write!(f, " ({})", codes.join(", "))?;
        }
        Ok(())
    }
}

/// When the base a guest printed `read` for stops being usable: `None` when it never does.
///
/// `read` is the marker line the first-logon script wrote to the serial port; text after the JSON is ignored.
/// A licensed product with grace left ends `now` plus that grace. An `evaluation` image in any other state is
/// `unavailable`: Windows did not license it, whatever the guest's activation said, and the error names that account
/// and the status. Any other image has no expiry, including one that reads no product with a key (`null`: a retail
/// install nobody activated). An evaluation image that reads none, and a read that is neither `null` nor the marker and
/// its fields, are `internal` errors: a build never goes on without knowing.
///
/// The host branches on the status and the grace only. A base that ends is `expired-base` only through the store's
/// margin on a licensed evaluation; what an evaluation that has really ended reads after `Activate` is not measured.
pub(super) fn expiry_from(read: &str, evaluation: bool, now: SystemTime) -> Result<Option<SystemTime>> {
    let Some(license) = parse(read)? else {
        return if evaluation {
            Err(unreadable(read, "an evaluation image reads no product with a key"))
        } else {
            Ok(None)
        };
    };
    if license.license_status == LICENSED && license.grace_period_remaining > 0 {
        let end = license
            .grace_period_remaining
            .checked_mul(60)
            .and_then(|seconds| now.checked_add(Duration::from_secs(seconds)))
            .ok_or_else(|| unreadable(read, "the grace period does not fit a date"))?;
        return Ok(Some(end));
    }
    if !evaluation {
        return Ok(None);
    }
    let account = match &license.activation {
        Some(activation) => format!("activation {activation}"),
        None => "no activation was tried".to_owned(),
    };
    Err(Error::new(
        ErrorKind::Unavailable,
        format!(
            "Windows did not license the evaluation: {account}; status {}",
            license.license_status
        ),
    ))
}

/// What `read` says, for a progress line: the numbers and the activation, never the marker line itself. A read that
/// cannot be read says why.
pub(super) fn describe(read: &str) -> String {
    match parse(read) {
        Ok(Some(license)) => {
            let mut text = format!(
                "license status {}, {} minutes of grace",
                license.license_status, license.grace_period_remaining
            );
            if let Some(activation) = &license.activation {
                text.push_str(&format!(", activation {activation}"));
            }
            text
        }
        Ok(None) => "no licensed product".to_owned(),
        Err(error) => error.message().to_owned(),
    }
}

/// The read's product, or `None` for the script's `null`: no product has a key.
fn parse(read: &str) -> Result<Option<License>> {
    let json = read
        .trim_start()
        .strip_prefix(MARKER)
        .ok_or_else(|| unreadable(read, "it does not start with the marker"))?;
    serde_json::Deserializer::from_str(json)
        .into_iter::<Option<License>>()
        .next()
        .ok_or_else(|| unreadable(read, "it holds no JSON"))?
        .map_err(|e| unreadable(read, &e.to_string()))
}

fn unreadable(read: &str, why: &str) -> Error {
    Error::new(
        ErrorKind::Internal,
        format!("cannot read the guest's {MARKER} line ({why}): {read:?}"),
    )
}
