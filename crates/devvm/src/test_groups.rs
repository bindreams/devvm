//! Test groups: one declaration each.
//!
//! A group names an environment a test needs and not every host has. It is a skuld fixture carrying three things:
//! `requires` (the off-switch: `Err` only for `DEVVM_TEST_<GROUP>=0`, so an opted-out test is ignored, at list time
//! too), a setup that fails the test when the host lacks what the group needs (a `precondition`) or when
//! `DEVVM_TEST_<GROUP>_CONSENT` is not exactly `1` (a `consent` group, one that touches real system state), and the
//! group's label (lane selection, `SKULD_LABELS=<label>`, declared in `test_harness.rs`).
//!
//! Every group is on by default. A test joins with `#[fixture(<fixture>)] _group: &Group`. CI turns the groups a job
//! cannot run off workflow-wide and each lane opts the ones it runs back in (`.github/workflows/ci.yaml`).

#[path = "test_groups_tests.rs"]
mod test_groups_tests;

use crate::host;

/// Proof that the group is enabled (and consented to, where asked).
pub(crate) struct Group;

/// The rule behind every consent group: `Ok(false)` for `enabled_var=0`; `Err` naming the consent variable for an
/// enabled group without exactly `<enabled_var>_CONSENT=1`.
pub(crate) fn check_group(enabled_var: &str, var: impl Fn(&str) -> Option<String>) -> Result<bool, String> {
    let consent = format!("{enabled_var}_CONSENT");
    if require_enabled(enabled_var, &var).is_err() {
        return Ok(false);
    }
    if var(&consent).as_deref() != Some("1") {
        return Err(format!(
            "tests under {enabled_var} touch real system state and run only in a sandbox (a container, VM or \
             CI); set {consent}=1 there to consent, or {enabled_var}=0 to turn the group off"
        ));
    }
    Ok(true)
}

/// A group's `requires`: `Err` only when `enabled_var` is `0`.
pub(crate) fn require_enabled(enabled_var: &str, var: impl Fn(&str) -> Option<String>) -> Result<(), String> {
    if var(enabled_var).as_deref() == Some("0") {
        return Err(format!("{enabled_var}=0"));
    }
    Ok(())
}

/// A consent group's setup: [`check_group`]'s rule, where `what` says what the group does.
pub(crate) fn require_consent(
    enabled_var: &str,
    what: &str,
    var: impl Fn(&str) -> Option<String>,
) -> Result<Group, String> {
    match check_group(enabled_var, var) {
        Ok(true) => Ok(Group),
        // skuld runs the body of a test whose `requires` failed when ignored tests are included
        // (`--run-ignored only`), so a group that is off refuses here as well.
        Ok(false) => Err(format!("{enabled_var}=0")),
        Err(why) => Err(format!("the group {what}: {why}")),
    }
}

/// Declares a test group: the fixture `$fixture`, labelled `$label` (declared in `test_harness.rs`), switched off by
/// `env = "<NAME>"` set to `0`. With `consent = "<what the group does>"` its setup also fails the test unless
/// `<NAME>_CONSENT` is exactly `1`. With `precondition = <fn>` it fails the test with the `Err` of `<fn>(<NAME>)`, which
/// names what the host lacks and the opt-out. Without either, a host that lacks what the group needs fails the test
/// with whatever it produces.
///
/// A test file brings the fixture into scope with `use crate::test_groups::{$fixture, Group};`. The module `$fixture`
/// holds `enabled()`, for code that picks what to run by the group (`Ok` unless `<NAME>` is `0`).
macro_rules! test_group {
    ($label:ident => $fixture:ident, env = $env:literal, consent = $what:literal) => {
        test_group!(@declare $label, $fixture, $env, require_consent($env, $what, |name| std::env::var(name).ok()));
    };
    ($label:ident => $fixture:ident, env = $env:literal, precondition = $check:path) => {
        test_group!(@declare $label, $fixture, $env,
            require_enabled($env, |name| std::env::var(name).ok()).and_then(|()| $check($env)).map(|()| Group));
    };
    ($label:ident => $fixture:ident, env = $env:literal) => {
        test_group!(@declare $label, $fixture, $env, require_enabled($env, |name| std::env::var(name).ok()).map(|()| Group));
    };
    (@declare $label:ident, $fixture:ident, $env:literal, $setup:expr) => {
        pub(crate) mod $fixture {
            #[allow(dead_code, reason = "a group's tests may not use enabled()")]
            pub(crate) fn enabled() -> Result<(), String> {
                super::require_enabled($env, |name| std::env::var(name).ok())
            }
        }

        // Process scope: `qemu_sidecar` (process scope) depends on `vm_group`, and skuld lets a fixture depend only on one of
        // the same or wider scope. A group holds no state, so one setup per process is all it needs.
        #[skuld::fixture(scope = process, requires = [$fixture::enabled], labels = [crate::test_harness::$label])]
        pub(crate) fn $fixture() -> Result<Group, String> {
            $setup
        }
    };
}

test_group!(VM => vm_group, env = "DEVVM_TEST_VM", consent = "boots QEMU under the host's hypervisor and downloads the pinned QEMU build");
test_group!(PYYAML => pyyaml, env = "DEVVM_TEST_PYYAML");
test_group!(OPENSSH_SFTP => openssh_sftp, env = "DEVVM_TEST_OPENSSH_SFTP");
test_group!(NTFS_NAMES => ntfs_names, env = "DEVVM_TEST_NTFS_NAMES");
test_group!(ACP_1252 => acp_1252, env = "DEVVM_TEST_ACP_1252", precondition = host_has_code_page_1252);
test_group!(UNICODE_SOCKET_DIR => unicode_socket_dir, env = "DEVVM_TEST_UNICODE_SOCKET_DIR", precondition = socket_dir_fits_unicode);

/// The `ACP_1252` group's precondition: this process's ANSI code page is 1252.
fn host_has_code_page_1252(enabled_var: &str) -> Result<(), String> {
    requires_code_page_1252(enabled_var, host::current().ansi_code_page())
}

/// [`host_has_code_page_1252`] for the code page `acp` (`None` on a host without one).
pub(crate) fn requires_code_page_1252(enabled_var: &str, acp: Option<u32>) -> Result<(), String> {
    match acp {
        Some(1252) => Ok(()),
        Some(other) => Err(format!(
            "the group needs ANSI code page 1252, and this host's is {other}; set {enabled_var}=0 to turn it off"
        )),
        None => Err(format!(
            "the group needs ANSI code page 1252, and this host has no ANSI code page; set {enabled_var}=0 to turn it off"
        )),
    }
}

/// The `UNICODE_SOCKET_DIR` group's precondition: [`unicode_socket_dir_fits`], with the opt-out added.
///
/// [`unicode_socket_dir_fits`]: crate::test_support::qemu::unicode_socket_dir_fits
fn socket_dir_fits_unicode(enabled_var: &str) -> Result<(), String> {
    crate::test_support::qemu::unicode_socket_dir_fits()
        .map_err(|why| format!("the group needs a socket directory with a space and a non-ASCII character: {why}; set {enabled_var}=0 to turn it off"))
}
