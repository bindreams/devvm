use crate::test_groups::{check_group, require_consent, require_enabled, requires_code_page_1252};
use crate::test_harness::{ACP_1252, NTFS_NAMES, OPENSSH_SFTP, PYYAML, UNICODE_SOCKET_DIR, VM};

fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name| vars.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string())
}

/// Mutant: the group variable is required to be set.
#[skuld::test]
fn an_unset_group_variable_with_consent_runs() {
    assert_eq!(
        check_group("DEVVM_TEST_VM", env(&[("DEVVM_TEST_VM_CONSENT", "1")])),
        Ok(true)
    );
}

/// Mutant: a set group variable counts as consent.
#[skuld::test]
fn group_on_without_consent_fails_and_names_the_consent_variable() {
    for vars in [&[("DEVVM_TEST_VM", "1")][..], &[("DEVVM_TEST_VM", "2")]] {
        let why = check_group("DEVVM_TEST_VM", env(vars)).unwrap_err();
        assert!(why.contains("DEVVM_TEST_VM_CONSENT=1"), "{why}");
        let why = require_consent("DEVVM_TEST_VM", "does a thing", env(vars))
            .err()
            .expect("no consent");
        assert!(why.contains("DEVVM_TEST_VM_CONSENT=1"), "{why}");
    }
}

/// Mutant: only `1` enables.
#[skuld::test]
fn any_group_value_but_0_runs() {
    let vars = [("DEVVM_TEST_VM", "2"), ("DEVVM_TEST_VM_CONSENT", "1")];
    assert_eq!(check_group("DEVVM_TEST_VM", env(&vars)), Ok(true));
}

/// Mutant: `0` is ignored.
#[skuld::test]
fn group_0_turns_the_group_off() {
    let vars = [("DEVVM_TEST_VM", "0"), ("DEVVM_TEST_VM_CONSENT", "1")];
    assert_eq!(check_group("DEVVM_TEST_VM", env(&vars)), Ok(false));
}

/// Consent is asked only of an enabled group. Mutant: consent is checked before `=0`.
#[skuld::test]
fn group_0_needs_no_consent() {
    assert_eq!(check_group("DEVVM_TEST_VM", env(&[("DEVVM_TEST_VM", "0")])), Ok(false));
}

/// Mutant: consent defaults to given.
#[skuld::test]
fn unset_consent_fails_and_names_the_variables() {
    let why = check_group("DEVVM_TEST_VM", env(&[])).unwrap_err();
    assert!(
        why.contains("DEVVM_TEST_VM_CONSENT=1") && why.contains("DEVVM_TEST_VM=0"),
        "{why}"
    );
}

/// Mutant: any non-empty consent counts.
#[skuld::test]
fn consent_other_than_1_fails() {
    for value in ["0", "yes", "true", " 1", ""] {
        let vars = [("DEVVM_TEST_VM_CONSENT", value)];
        assert!(
            check_group("DEVVM_TEST_VM", env(&vars)).is_err(),
            "consent {value:?} was accepted"
        );
    }
}

/// Mutant: `require_enabled` asks for consent as well, so a group lacking only consent is ignored instead of failing at
/// setup.
#[skuld::test]
fn require_enabled_fails_only_for_0() {
    assert!(require_enabled("DEVVM_TEST_X", env(&[])).is_ok());
    for value in ["", "1", "2", "yes", "false", "00"] {
        assert!(
            require_enabled("DEVVM_TEST_X", env(&[("DEVVM_TEST_X", value)])).is_ok(),
            "{value:?}"
        );
    }
    let why = require_enabled("DEVVM_TEST_X", env(&[("DEVVM_TEST_X", "0")])).unwrap_err();
    assert!(why.contains("DEVVM_TEST_X=0"), "{why}");
}

/// Mutant: setup grants without consent.
#[skuld::test]
fn require_consent_grants_only_on_exactly_1() {
    assert!(require_consent("DEVVM_TEST_X", "does a thing", env(&[("DEVVM_TEST_X_CONSENT", "1")])).is_ok());
    let off = [("DEVVM_TEST_X", "0"), ("DEVVM_TEST_X_CONSENT", "1")];
    let why = require_consent("DEVVM_TEST_X", "does a thing", env(&off))
        .err()
        .expect("a group that is off");
    assert!(why.contains("DEVVM_TEST_X=0"), "{why}");
    for vars in [&[][..], &[("DEVVM_TEST_X_CONSENT", "yes")]] {
        let why = require_consent("DEVVM_TEST_X", "does a thing", env(vars))
            .err()
            .expect("no consent");
        assert!(
            why.contains("does a thing") && why.contains("DEVVM_TEST_X_CONSENT=1"),
            "{why}"
        );
    }
}

/// Mutant: any other code page passes, or a host without one does.
#[skuld::test]
fn the_code_page_group_passes_only_at_1252_and_names_what_the_host_has() {
    assert_eq!(requires_code_page_1252("DEVVM_TEST_ACP_1252", Some(1252)), Ok(()));
    let why = requires_code_page_1252("DEVVM_TEST_ACP_1252", Some(65001)).unwrap_err();
    assert!(
        why.contains("1252") && why.contains("65001") && why.contains("DEVVM_TEST_ACP_1252=0"),
        "{why}"
    );
    let why = requires_code_page_1252("DEVVM_TEST_ACP_1252", None).unwrap_err();
    assert!(why.contains("1252") && why.contains("DEVVM_TEST_ACP_1252=0"), "{why}");
}

/// Mutant: a row's fixture carries no label, or another row's, so `SKULD_LABELS=<label>` selects none of its tests (or
/// someone else's).
#[skuld::test]
fn every_group_fixture_carries_exactly_its_label() {
    for (fixture, label) in [
        ("vm_group", VM),
        ("pyyaml", PYYAML),
        ("openssh_sftp", OPENSSH_SFTP),
        ("ntfs_names", NTFS_NAMES),
        ("acp_1252", ACP_1252),
        ("unicode_socket_dir", UNICODE_SOCKET_DIR),
    ] {
        assert_eq!(skuld::fixture::collect_fixture_labels(&[fixture]), [label], "{fixture}");
    }
}
