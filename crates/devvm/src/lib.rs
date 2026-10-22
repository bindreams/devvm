//! devvm: disposable virtual machines for tests.

pub mod error;
pub mod types;

pub use error::{Error, ErrorKind, Result};
pub use types::{Arch, BuildOptions, Command, GuestKind, Network, RunAs, Sha256, StartOptions};

/// The target triple this crate was compiled for, e.g. `x86_64-pc-windows-msvc`.
pub const TARGET: &str = env!("DEVVM_TARGET");

#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;

#[cfg(test)]
fn main() {
    skuld::run_all();
}
