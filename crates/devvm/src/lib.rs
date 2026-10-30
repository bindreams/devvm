//! devvm: disposable virtual machines for tests.

#[allow(dead_code, reason = "unused until the QEMU backend lands")]
pub(crate) mod access;
#[allow(dead_code, reason = "unused until the QEMU backend lands")]
pub(crate) mod backend;
pub mod config;
#[allow(dead_code, reason = "unused until the QEMU backend lands")]
pub(crate) mod contain;
pub mod dirs;
#[allow(dead_code, reason = "unused until the QEMU backend lands")]
pub mod engine;
pub mod error;
pub mod exit;
#[allow(dead_code, reason = "unused until the QEMU backend lands")]
pub(crate) mod guest;
#[allow(dead_code, reason = "unused until the QEMU backend lands")]
pub(crate) mod host;
#[allow(dead_code, reason = "unused until the QEMU backend lands")]
pub(crate) mod instance;
#[allow(dead_code, reason = "unused until the QEMU backend lands")]
pub(crate) mod media;
pub mod recipe;
pub mod store;
pub mod types;
pub mod vm;

pub use dirs::Dirs;
pub use engine::{Engine, ListFilter};
pub use error::{Error, ErrorKind, Result};
pub use exit::{ExitStatus, Output};
pub use host::{Unavailable, host_exit_width};
pub use instance::info::{InstanceInfo, InstanceState};
pub use types::{Arch, BuildOptions, Command, GuestKind, Network, RunAs, Sha256, StartOptions};
pub use vm::{Guest, Image, Instance, Vm, availability};

/// The target triple this crate was compiled for, e.g. `x86_64-pc-windows-msvc`.
pub const TARGET: &str = env!("DEVVM_TARGET");

#[cfg(test)]
mod test_support;

#[cfg(test)]
fn main() {
    test_helpers::run_if_requested();
    skuld::run_all();
}

/// Registry of the lib's re-exec helpers (see `test_support::helper`).
#[cfg(test)]
mod test_helpers {
    use crate::test_support::helper::{self, HelperFn};

    const HELPERS: &[(&str, HelperFn)] = &[
        ("hold-lock", crate::store::lock::lock_tests::hold_lock),
        ("sleeper", crate::contain::contain_tests::sleeper),
        ("fake-vm", crate::test_support::fake_vm::fake_vm),
        ("fake-launcher", crate::backend::fake::fake_launcher),
        ("fake-shutdown", crate::backend::fake::fake_shutdown),
        ("fake-cli-start", crate::vm::vm_tests::fake_cli_start),
        ("fake-cli-run", crate::vm::vm_tests::fake_cli_run),
        ("fake-cli-up", crate::vm::vm_tests::fake_cli_up),
        ("contain-owner", crate::test_support::fake_vm::contain_owner),
        ("release-owner", crate::test_support::fake_vm::release_owner),
        ("console-window", crate::test_support::fake_vm::console_window),
        ("layer-builder", crate::store::layers::cross_tests::layer_builder),
        ("lease-holder", crate::store::layers::gc_tests::lease_holder),
        ("fetch-artifact", crate::store::fetch::http_tests::fetch_artifact),
    ];

    pub fn run_if_requested() {
        helper::run_if_requested(HELPERS);
    }
}
