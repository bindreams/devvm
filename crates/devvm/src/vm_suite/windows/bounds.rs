//! The failure bounds of the Windows suites, besides [`ANSWER_BOUND`] (`vm_suite/waits.rs`). Each fails the test with a
//! message that names what did not happen; none is taken as a fact about the guest.

use std::time::Duration;

/// A condition in the guest that the host re-checks: a flag file, a process that ends, a request that is removed, a
/// reconnect, a line from a command.
pub const STATE_BOUND: Duration = Duration::from_secs(5 * 60);

/// The output of a terminal session that one wait expects, from the start of that wait.
pub const TERMINAL_BOUND: Duration = Duration::from_secs(5 * 60);

/// A restart of an instance: twice the engine's own bound on a booting guest.
pub const BOOT_BOUND: Duration = Duration::from_secs(20 * 60);
