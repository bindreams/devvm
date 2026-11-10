//! Test groups: an environment a test needs and not every host has. Each is a skuld fixture, on unless
//! `DEVVM_TEST_<GROUP>=0` (the test is then ignored). Declared with `test_group!`; a test joins with
//! `#[fixture(<fixture>)] _group: &Group`.
//!
//! This half needs nothing from the lib, so a test binary can include it by `#[path]`, with `test_harness.rs`, which
//! declares the labels the groups carry. The groups that call the lib are in `test_groups.rs`.

/// Proof that the group is enabled.
pub(crate) struct Group;

/// Consent rule: `Ok(false)` if the group is off, `Err` unless `<enabled_var>_CONSENT` is `1`.
pub(crate) fn check_group(
    enabled_var: &str,
    opt_out: &str,
    var: impl Fn(&str) -> Option<String>,
) -> Result<bool, String> {
    let consent = format!("{enabled_var}_CONSENT");
    if require_enabled(enabled_var, &var).is_err() {
        return Ok(false);
    }
    if var(&consent).as_deref() != Some("1") {
        return Err(format!(
            "tests under {enabled_var} touch real system state and run only in a sandbox (a container, VM or \
             CI); set {consent}=1 there to consent, or {opt_out}"
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
    opt_out: &str,
    what: &str,
    var: impl Fn(&str) -> Option<String>,
) -> Result<Group, String> {
    match check_group(enabled_var, opt_out, var) {
        Ok(true) => Ok(Group),
        // skuld runs the fixture of an ignored test under `--run-ignored only`, so an off group must fail here too.
        Ok(false) => Err(format!("{enabled_var}=0")),
        Err(why) => Err(format!("the group {what}: {why}")),
    }
}

/// Declares a test group: the fixture `$fixture`, labelled `$label` (declared in `test_harness.rs`), switched off by
/// `env = "<NAME>"` set to `0`. With `consent = "<what the group does>"` its setup also fails the test unless
/// `<NAME>_CONSENT` is exactly `1`. With `precondition = <fn>` it fails the test with the `Err` of `<fn>(OPT_OUT)`,
/// which names what the host lacks and the opt-out (except in the child `PINNED_CHILD` marks, which cannot opt out).
/// Without either, a host that lacks what the group needs fails the test with whatever it produces.
///
/// The module `$fixture` holds `enabled()`, for code that picks what to run by the group (`Ok` unless `<NAME>` is `0`),
/// and `OPT_OUT`, the words a failure message uses to say how to turn the group off.
macro_rules! test_group {
    ($label:ident => $fixture:ident, env = $env:literal, consent = $what:literal) => {
        test_group!(@declare $label, $fixture, $env,
            require_consent($env, $fixture::OPT_OUT, $what, |name| std::env::var(name).ok()));
    };
    ($label:ident => $fixture:ident, env = $env:literal, precondition = $check:path) => {
        test_group!(@declare $label, $fixture, $env,
            require_enabled($env, |name| std::env::var(name).ok())
                .and_then(|()| $check($fixture::OPT_OUT))
                .map(|()| Group));
    };
    ($label:ident => $fixture:ident, env = $env:literal) => {
        test_group!(@declare $label, $fixture, $env,
            require_enabled($env, |name| std::env::var(name).ok()).map(|()| Group));
    };
    (@declare $label:ident, $fixture:ident, $env:literal, $setup:expr) => {
        pub(crate) mod $fixture {
            /// How to turn the group off.
            #[allow(dead_code, reason = "a group whose failures do not name it, or a binary with no test in it, leaves it unused")]
            pub(crate) const OPT_OUT: &str = concat!("set ", $env, "=0 to turn the group off");

            #[allow(dead_code, reason = "a group's tests may not use enabled()")]
            pub(crate) fn enabled() -> Result<(), String> {
                super::require_enabled($env, |name| std::env::var(name).ok())
            }
        }

        // Process scope: skuld lets a fixture depend only on one of the same or wider scope, and process-scoped
        // fixtures (`qemu_sidecar`) depend on groups.
        #[skuld::fixture(scope = process, requires = [$fixture::enabled], labels = [crate::test_harness::$label])]
        pub(crate) fn $fixture() -> Result<Group, String> {
            $setup
        }
    };
}

test_group!(
    VM => vm_group,
    env = "DEVVM_TEST_VM",
    consent = "boots QEMU under the host's hypervisor and downloads the pinned QEMU build"
);
// Windows only uses it: the test attaches a virtual disk with diskpart, which needs an elevated token.
test_group!(
    EXFAT_VOLUME => exfat_volume,
    env = "DEVVM_TEST_EXFAT_VOLUME",
    consent = "attaches a virtual disk to the host with diskpart and formats it exFAT"
);
test_group!(PYYAML => pyyaml, env = "DEVVM_TEST_PYYAML");
test_group!(OPENSSH_SFTP => openssh_sftp, env = "DEVVM_TEST_OPENSSH_SFTP");
test_group!(NTFS_NAMES => ntfs_names, env = "DEVVM_TEST_NTFS_NAMES");
// Only Windows can lack the right to make a symlink (Developer Mode or an elevated token); every OS declares the group.
test_group!(SYMLINKS => symlinks, env = "DEVVM_TEST_SYMLINKS");
// The VM tests' base directory: `C:\Users` where on, the temporary directory where off. Picked in
// `vm_suite/fixture.rs::base_dir`.
test_group!(UNICODE_BASE => unicode_base, env = "DEVVM_TEST_UNICODE_BASE");
