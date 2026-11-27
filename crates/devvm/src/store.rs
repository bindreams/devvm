//! The stores under the data directory, and the time format of their state files.

use std::time::SystemTime;

use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "store_tests.rs"]
mod store_tests;

pub mod fetch;
pub mod layers;
pub mod lock;
pub(crate) mod quarantine;

/// An instant as the RFC 3339 text a state file holds.
pub(crate) fn time_text(time: SystemTime) -> Result<String> {
    jiff::Timestamp::try_from(time)
        .map(|timestamp| timestamp.to_string())
        .map_err(|e| Error::new(ErrorKind::Store, format!("time out of range: {e}")))
}

/// The instant an RFC 3339 timestamp names. The error names the text, whether it is no timestamp or one this platform's
/// `SystemTime` cannot hold.
pub(crate) fn time_parse(text: &str) -> std::result::Result<SystemTime, String> {
    text.parse::<jiff::Timestamp>()
        .and_then(SystemTime::try_from)
        .map_err(|e| format!("bad time `{text}`: {e}"))
}
