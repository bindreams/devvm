//! The warning for an instance whose guest has expired.

use std::time::SystemTime;

use crate::engine::Engine;
use crate::instance::name::InstanceName;
use crate::store::layers::date_text;

#[cfg(test)]
#[path = "expiry_tests.rs"]
pub(crate) mod expiry_tests;

/// Warns on the progress channel that `name`'s guest has expired. Never fails the caller.
pub(crate) fn warn_if_expired(engine: &Engine, name: &InstanceName, expires: Option<SystemTime>) {
    if let Some(expires) = expires
        && engine.clock().now() >= expires
    {
        engine.progress().message(&format!(
            "instance {name}: the guest expired on {} (UTC)",
            date_text(expires)
        ));
    }
}
