//! The failure bounds of the Windows suites, besides [`ANSWER_BOUND`] (`vm_suite/waits.rs`). Each fails the test with a
//! message that names what did not happen; none is taken as a fact about the guest.

use std::time::Duration;

use crate::vm_suite::waits::ANSWER_BOUND;

/// A condition in the guest that the host re-checks: a flag file, a process that ends, a request that is removed, a
/// reconnect, a line from a command.
pub const STATE_BOUND: Duration = Duration::from_secs(5 * 60);

/// The next output of a terminal session.
pub const TERMINAL_BOUND: Duration = Duration::from_secs(5 * 60);

/// A restart of an instance: twice the engine's own bound on a booting guest.
pub const BOOT_BOUND: Duration = Duration::from_secs(20 * 60);

/// A wait that runs in the guest: twice the longest host bound that waits on its effect, [`ANSWER_BOUND`], so the host
/// fails the test first. It only ends a wait that nobody is left to end.
pub const GUEST_BOUND: Duration = Duration::from_secs(2 * ANSWER_BOUND.as_secs());
