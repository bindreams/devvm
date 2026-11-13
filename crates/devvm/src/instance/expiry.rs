//! The warning for an instance whose guest has expired.

use std::time::SystemTime;

use crate::engine::Engine;
use crate::instance::name::InstanceName;
use crate::store::layers::date_text;

#[cfg(test)]
#[path = "expiry_tests.rs"]
mod expiry_tests;

/// Says on the progress channel that the guest of instance `name` has expired, when its stored `expires` is not after
/// the engine's clock. An instance without an expiry never warns.
pub(crate) fn warn_if_expired(engine: &Engine, name: &InstanceName, expires: Option<SystemTime>) {
    if let Some(expires) = expires
        && engine.clock().now() >= expires
    {
        engine
            .progress()
            .message(&format!("instance {name}: the guest expired on {}", date_text(expires)));
    }
}
