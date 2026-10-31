//! The gate every VM-booting test goes through.
//!
//! A VM test never skips silently. It runs by default and fails, naming the missing piece, when QEMU or hardware
//! acceleration is unavailable. The only opt-out is the explicit switch `DEVVM_SKIP_VM_TESTS=1`, which CI jobs that
//! cannot boot VMs set.
//!
//! The pattern, for a test target that includes this file with `#[path]`:
//!
//! ```ignore
//! #[skuld::fixture(requires = [vm_tests_enabled])]
//! fn vm_host() -> Result<VmHost, String> {
//!     // Err names the missing piece, e.g. the message of `devvm::availability()`.
//! }
//! ```
//!
//! skuld ignores the test (listing the reason) only when `vm_tests_enabled` returns `Err`, which this module does
//! only for the opt-out. With the switch unset, a failing `vm_host` setup fails the test.

/// The label of every test that boots QEMU or runs one of its tools. `SKULD_LABELS=vm` selects exactly these, and
/// nothing selects by test name.
#[skuld::label]
pub const VM: skuld::Label;

/// The environment variable that opts VM tests out.
pub const SKIP_VAR: &str = "DEVVM_SKIP_VM_TESTS";

/// `Err` exactly when `skip_var` is `Some("1")`; any other value, including `0`, `true` and empty, keeps the tests on.
pub fn gate(skip_var: Option<&str>) -> Result<(), String> {
    match skip_var {
        Some("1") => Err(format!("{SKIP_VAR}=1")),
        _ => Ok(()),
    }
}

/// A skuld precondition (`requires = [vm_tests_enabled]`) reading the real environment.
pub fn vm_tests_enabled() -> Result<(), String> {
    gate(std::env::var(SKIP_VAR).ok().as_deref())
}
