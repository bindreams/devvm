//! Instance names: lowercase ASCII, safe as a directory name on every OS and as an SSH host pattern.

use std::fmt;

use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "name_tests.rs"]
mod name_tests;

/// Windows device names that cannot be file names; `com1` to `com9` and `lpt1` to `lpt9` follow the same rule.
const RESERVED: [&str; 4] = ["con", "prn", "aux", "nul"];

/// A validated instance name: `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`, not a Windows device name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct InstanceName(String);

impl InstanceName {
    /// ASCII-lowercases `text` and validates it. Lowercasing is ASCII-only: Unicode lowercasing maps the KELVIN SIGN to
    /// `k`, which would accept non-ASCII input and name a directory differently from the typed text.
    pub(crate) fn parse(text: &str) -> Result<InstanceName> {
        let lower = text.to_ascii_lowercase();
        let invalid = |why: &str| {
            Error::new(
                ErrorKind::InvalidName,
                format!("{text:?} is not a valid instance name: {why}"),
            )
        };
        if lower.is_empty() || lower.len() > 63 {
            return Err(invalid("it must be 1 to 63 characters"));
        }
        let edge = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
        if !lower.chars().all(|c| edge(c) || c == '-') {
            return Err(invalid("it may contain only the letters a-z, digits and '-'"));
        }
        if !lower.starts_with(edge) || !lower.ends_with(edge) {
            return Err(invalid("it must start and end with a letter or digit"));
        }
        if is_device_name(&lower) {
            return Err(invalid("it is a reserved Windows device name"));
        }
        Ok(InstanceName(lower))
    }

    /// A fresh name, `run-` and eight random lowercase hex digits.
    pub(crate) fn generate() -> InstanceName {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).expect("the OS random number generator is available");
        InstanceName(format!("run-{}", hex::encode(bytes)))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_device_name(name: &str) -> bool {
    if RESERVED.contains(&name) {
        return true;
    }
    for prefix in ["com", "lpt"] {
        if let Some(digit) = name.strip_prefix(prefix)
            && matches!(digit.as_bytes(), [b'1'..=b'9'])
        {
            return true;
        }
    }
    false
}

impl fmt::Display for InstanceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
