//! Test groups: an environment a test needs and not every host has. Each is a skuld fixture, on unless
//! `DEVVM_TEST_<GROUP>=0` (the test is then ignored). Declared with `test_group!`; a test joins with
//! `#[fixture(<fixture>)] _group: &Group`.

use crate::host;

/// Set, to its manifest's locale, in the child `host_tests` pins to code page 1252: stops it from spawning another.
pub(crate) const PINNED_CHILD: &str = "DEVVM_PINNED_CHILD";

/// Proof that the group is enabled.
pub(crate) struct Group;

/// Consent rule: `Ok(false)` if the group is off, `Err` unless `<enabled_var>_CONSENT` is `1`.
pub(crate) fn check_group(enabled_var: &str, var: impl Fn(&str) -> Option<String>) -> Result<bool, String> {
    let consent = format!("{enabled_var}_CONSENT");
    if require_enabled(enabled_var, &var).is_err() {
        return Ok(false);
    }
    if var(&consent).as_deref() != Some("1") {
        return Err(format!(
            "tests under {enabled_var} touch real system state and run only in a sandbox (a container, VM or \
             CI); set {consent}=1 there to consent, or set {enabled_var}=0 to turn the group off"
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
        // skuld runs the fixture of an ignored test under `--run-ignored only`, so an off group must fail here too.
        Ok(false) => Err(format!("{enabled_var}=0")),
        Err(why) => Err(format!("the group {what}: {why}")),
    }
}

/// Declares a test group: the fixture `$fixture`, labelled `$label` (declared in `test_harness.rs`), switched off by
/// `env = "<NAME>"` set to `0`. With `consent = "<what the group does>"` its setup also fails the test unless
/// `<NAME>_CONSENT` is exactly `1`. With `precondition = <fn>` it fails the test with the `Err` of `<fn>(OPT_OUT)`,
/// which names what the host lacks and ends with the opt-out. Without either, a host that lacks what the group needs
/// fails the test with whatever it produces.
///
/// The module `$fixture` holds `enabled()`, for code that picks what to run by the group (`Ok` unless `<NAME>` is `0`),
/// and `OPT_OUT`, the words a failure message uses to say how to turn the group off.
macro_rules! test_group {
    ($label:ident => $fixture:ident, env = $env:literal, consent = $what:literal) => {
        test_group!(@declare $label, $fixture, $env, require_consent($env, $what, |name| std::env::var(name).ok()));
    };
    ($label:ident => $fixture:ident, env = $env:literal, precondition = $check:path) => {
        test_group!(@declare $label, $fixture, $env,
            require_enabled($env, |name| std::env::var(name).ok()).and_then(|()| $check($fixture::OPT_OUT)).map(|()| Group));
    };
    ($label:ident => $fixture:ident, env = $env:literal) => {
        test_group!(@declare $label, $fixture, $env, require_enabled($env, |name| std::env::var(name).ok()).map(|()| Group));
    };
    (@declare $label:ident, $fixture:ident, $env:literal, $setup:expr) => {
        pub(crate) mod $fixture {
            /// How to turn the group off; every message that fails a test of the group ends with it.
            pub(crate) const OPT_OUT: &str = concat!("set ", $env, "=0 to turn the group off");

            #[allow(dead_code, reason = "a group's tests may not use enabled()")]
            pub(crate) fn enabled() -> Result<(), String> {
                super::require_enabled($env, |name| std::env::var(name).ok())
            }
        }

        // Process scope: skuld lets a fixture depend only on one of the same or wider scope, and process-scoped fixtures
        // (`qemu_sidecar`) depend on groups.
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

fn host_has_code_page_1252(opt_out: &str) -> Result<(), String> {
    let code_page = host::current().ansi_code_page();
    if code_page == Some(1252) {
        return Ok(());
    }
    if std::env::var_os(PINNED_CHILD).is_some() {
        return Err(format!(
            "the manifest's activeCodePage did not pin the process to ANSI code page 1252; its code page is {code_page:?}"
        ));
    }
    match code_page {
        Some(other) => Err(format!(
            "the group needs ANSI code page 1252, and this host's is {other}; {opt_out}"
        )),
        None => Err(format!(
            "the group needs ANSI code page 1252, and this host has no ANSI code page; {opt_out}"
        )),
    }
}

fn socket_dir_fits_unicode(opt_out: &str) -> Result<(), String> {
    crate::test_support::qemu::unicode_socket_dir_fits().map_err(|why| {
        format!("the group needs a socket directory with a space and a non-ASCII character: {why}; {opt_out}")
    })
}
