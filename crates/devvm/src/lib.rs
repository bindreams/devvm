//! devvm: disposable virtual machines for tests.

#[allow(dead_code, reason = "unused until the QEMU backend lands")]
pub(crate) mod access;
#[allow(dead_code, reason = "unused until the QEMU backend lands")]
pub(crate) mod backend;
pub mod config;
#[allow(dead_code, reason = "unused until the QEMU backend lands")]
pub(crate) mod contain;
#[allow(dead_code, reason = "unused until the engine builds copy steps")]
pub(crate) mod copy;
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

/// The gate every VM-booting test goes through (shared with `tests/vm_gate.rs`).
#[cfg(test)]
#[path = "../tests/support/vm_gate.rs"]
mod vm_gate;

/// The tests that boot QEMU, and the contract suite against it.
#[cfg(test)]
mod vm_suite;

/// Counts live heap bytes for `test_support::alloc::peak_bytes`.
#[cfg(test)]
#[global_allocator]
static ALLOCATOR: test_support::alloc::CountingAllocator = test_support::alloc::CountingAllocator;

#[cfg(test)]
fn main() {
    test_helpers::run_if_requested();
    let mut runner = skuld::TestRunner::new();
    // A mistyped label in SKULD_LABELS must fail, not select nothing.
    runner.require_known_labels();
    runner.run();
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
        (
            "qemu-extract",
            crate::backend::qemu::sidecar::sidecar_tests::extract_and_wait,
        ),
        ("qemu-launcher", crate::vm_suite::backend::qemu_launcher),
        ("qemu-shutdown", crate::vm_suite::backend::qemu_shutdown),
        ("access-heap", crate::access::heap_tests::access_heap),
        ("snapshot-big", crate::copy::heap_tests::snapshot_big),
    ];

    pub fn run_if_requested() {
        helper::run_if_requested(HELPERS);
        // The environment of a `qemu-img` is its own, so a copy of this binary installed as one cannot be told to be a
        // helper by a variable.
        crate::backend::qemu::sidecar::sidecar_tests::act_as_qemu_img_when_installed_as_one();
    }
}
