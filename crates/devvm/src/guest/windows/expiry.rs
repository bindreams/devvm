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

/// The fields of the read that are numbers; the description and the family are only echoed in logs, so no localized
/// text is read.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct License {
    license_status: u32,
    /// Minutes until a time-based license ends.
    grace_period_remaining: u64,
}

/// When the base a guest printed `read` for stops being usable: `None` when it never does.
///
/// `read` is the marker line the first-logon script wrote to the serial port; text after the JSON is ignored.
/// A licensed product with grace left ends `now` plus that grace. An `evaluation` image in any other state is born
/// expired and ends `now` (the LTSC 2024 evaluation ISO reads status 5 with no grace on a fresh install). Any other
/// image has no expiry, including one that reads no product with a key (`null`: a retail install nobody activated).
/// An evaluation image that reads none, and a read that is neither `null` nor the marker and two numbers, are
/// `internal` errors: a build never goes on without knowing.
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
    Ok(evaluation.then_some(now))
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
