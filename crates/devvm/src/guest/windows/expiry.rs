//! The license read of a Windows base, and when it ends.

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

/// The start of the `Activation` field of an evaluation whose activation failed; the guest's message follows.
const ACTIVATION_FAILED: &str = "failed: ";

/// The `Activation` field of an evaluation whose activation succeeded.
const ACTIVATION_DONE: &str = "activated";

/// The fields of the read that are numbers, and the activation's outcome; the description and the family are only
/// echoed in logs, so no localized text is read except the guest's own account of a failed activation.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct License {
    license_status: u32,
    /// Minutes until a time-based license ends.
    grace_period_remaining: u64,
    /// What the activation of an unlicensed evaluation came to: `activated` or `failed: <the guest's message>`. Absent
    /// when none was tried.
    activation: Option<String>,
}

/// When the base a guest printed `read` for stops being usable: `None` when it never does.
///
/// `read` is the marker line the first-logon script wrote to the serial port; text after the JSON is ignored.
/// A licensed product with grace left ends `now` plus that grace. An `evaluation` image in any other state ends `now`:
/// it is born expired when its `Activation` is `activated` or absent (an unlicensed evaluation reads status 5 until
/// Windows activates it, so the guest activates it before the read). When that activation failed, the build is
/// `unavailable`: the evaluation's state is not known, and a later build tries again. Any other image has no expiry,
/// including one that reads no product with a key (`null`: a retail install nobody activated). An evaluation image
/// that reads none, a read that is neither `null` nor the marker and its fields, and an `Activation` that is
/// neither `activated` nor `failed: ...`, are `internal` errors: a build never goes on without knowing.
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
    match license.activation.as_deref() {
        None => {}
        Some(ACTIVATION_DONE) => {}
        Some(other) => match other.strip_prefix(ACTIVATION_FAILED) {
            Some(failure) => {
                return Err(Error::new(
                    ErrorKind::Unavailable,
                    format!(
                        "Windows could not activate the evaluation ({failure}); the build needs the network, and a later build tries again"
                    ),
                ));
            }
            None => {
                return Err(unreadable(
                    read,
                    &format!("the activation {other:?} is neither done nor failed"),
                ));
            }
        },
    }
    Ok(Some(now))
}

/// What `read` says, for a progress line: the numbers and the activation, never the marker line itself.
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
        Err(_) => "license cannot be read".to_owned(),
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
