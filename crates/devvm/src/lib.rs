//! devvm: disposable virtual machines for tests.

pub mod config;
pub mod dirs;
pub mod error;
pub mod exit;
pub mod recipe;
pub mod store;
pub mod types;

pub use dirs::Dirs;
pub use error::{Error, ErrorKind, Result};
pub use types::{Arch, BuildOptions, Command, GuestKind, Network, RunAs, Sha256, StartOptions};

/// The target triple this crate was compiled for, e.g. `x86_64-pc-windows-msvc`.
pub const TARGET: &str = env!("DEVVM_TARGET");

#[cfg(test)]
mod test_support;

#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;

#[cfg(test)]
fn main() {
    test_helpers::run_if_requested();
    skuld::run_all();
}

/// Registry of the lib's re-exec helpers (see `test_support::helper`).
#[cfg(test)]
mod test_helpers {
    use crate::test_support::helper::{self, HelperFn};

    const HELPERS: &[(&str, HelperFn)] = &[("hold-lock", crate::store::lock::lock_tests::hold_lock)];

    pub fn run_if_requested() {
        helper::run_if_requested(HELPERS);
    }
}
