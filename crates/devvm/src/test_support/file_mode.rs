//! Reading and setting Unix file modes from tests that run on every OS: off Unix there are no modes.

#[cfg(unix)]
#[path = "file_mode/unix.rs"]
mod imp;
#[cfg(not(unix))]
#[path = "file_mode/other.rs"]
mod imp;

pub use imp::{mode, set_mode};
