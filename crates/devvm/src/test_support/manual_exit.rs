//! A process whose exit a test decides: the stand-in for QEMU's process where a test needs the exit but not the process.

#[cfg(not(windows))]
#[path = "manual_exit/other.rs"]
mod platform;
#[cfg(windows)]
#[path = "manual_exit/windows.rs"]
mod platform;

pub use platform::ManualExit;
