use std::collections::HashMap;

use super::{ACP, BASH_5_2, Claim, NTFS_NAMES, OPENSSH_SFTP, PYYAML, STEP_VAR, VM_TESTS, missing};

const TEST: &str = "test";
const OPENSSH: &str = "openssh-sftp";
const VM: &str = "vm";

fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    let vars: HashMap<_, _> = vars.iter().copied().collect();
    move |name| vars.get(name).map(|value| (*value).to_owned())
}

fn names(claims: Result<Vec<&Claim>, String>) -> Vec<&'static str> {
    claims.unwrap().into_iter().map(|claim| claim.var).collect()
}

fn made(claims: &[&Claim]) -> Vec<(&'static str, &'static str)> {
    claims.iter().map(|claim| (claim.var, claim.value)).collect()
}

#[skuld::test]
fn the_test_step_on_linux_must_claim_pyyaml_and_bash_5_2() {
    assert_eq!(names(missing(TEST, "linux", env(&[]))), [PYYAML.var, BASH_5_2.var]);
    assert_eq!(names(missing(TEST, "linux", env(&made(&[&PYYAML])))), [BASH_5_2.var]);
    assert!(
        missing(TEST, "linux", env(&made(&[&PYYAML, &BASH_5_2])))
            .unwrap()
            .is_empty()
    );
}

#[skuld::test]
fn the_test_step_on_windows_must_claim_the_code_page_and_ntfs_names() {
    assert_eq!(names(missing(TEST, "windows", env(&[]))), [ACP.var, NTFS_NAMES.var]);
    assert_eq!(names(missing(TEST, "windows", env(&made(&[&ACP])))), [NTFS_NAMES.var]);
    assert!(
        missing(TEST, "windows", env(&made(&[&ACP, &NTFS_NAMES])))
            .unwrap()
            .is_empty()
    );
}

#[skuld::test]
fn the_openssh_step_must_claim_openssh_sftp_pyyaml_and_bash_5_2() {
    assert_eq!(
        names(missing(OPENSSH, "linux", env(&[]))),
        [OPENSSH_SFTP.var, PYYAML.var, BASH_5_2.var]
    );
    assert_eq!(
        names(missing(OPENSSH, "linux", env(&made(&[&PYYAML])))),
        [OPENSSH_SFTP.var, BASH_5_2.var]
    );
    assert!(
        missing(OPENSSH, "linux", env(&made(&[&OPENSSH_SFTP, &PYYAML, &BASH_5_2])))
            .unwrap()
            .is_empty()
    );
}

#[skuld::test]
fn a_claim_counts_only_with_its_documented_value() {
    for (step, os, claim) in [
        (TEST, "linux", &PYYAML),
        (TEST, "linux", &BASH_5_2),
        (TEST, "windows", &ACP),
        (TEST, "windows", &NTFS_NAMES),
        (OPENSSH, "linux", &OPENSSH_SFTP),
        (VM, "windows", &VM_TESTS),
    ] {
        // Every value but the claim's own is wrong: `0` is wrong for the others and right for the vm job.
        for wrong in ["", "0", "1", "yes", "true", "1 ", "01", "1253"]
            .into_iter()
            .filter(|wrong| *wrong != claim.value)
        {
            let absent = missing(step, os, env(&[(claim.var, wrong)])).unwrap();
            assert!(
                absent.iter().any(|owed| owed.var == claim.var),
                "{claim_var}={wrong:?}",
                claim_var = claim.var
            );
        }
    }
}

#[skuld::test]
fn a_step_owes_no_claim_it_does_not_cover() {
    // macOS and Windows runners have a bash that is not 5.2 (3.2 and Git's 5.3).
    assert!(missing(TEST, "macos", env(&[])).unwrap().is_empty());
    assert!(
        !missing(TEST, "windows", env(&[]))
            .unwrap()
            .iter()
            .any(|owed| owed.var == BASH_5_2.var)
    );
    assert!(
        !missing(TEST, "linux", env(&[]))
            .unwrap()
            .iter()
            .any(|owed| owed.var == OPENSSH_SFTP.var)
    );
    assert!(!ACP.is_made_in(env(&made(&[&PYYAML]))));
}

#[skuld::test]
fn the_vm_job_on_windows_must_claim_the_code_page_and_the_vm_tests() {
    assert_eq!(names(missing(VM, "windows", env(&[]))), [ACP.var, VM_TESTS.var]);
    assert_eq!(names(missing(VM, "windows", env(&made(&[&ACP])))), [VM_TESTS.var]);
    assert!(
        missing(VM, "windows", env(&made(&[&ACP, &VM_TESTS])))
            .unwrap()
            .is_empty()
    );
    assert!(missing(VM, "linux", env(&[])).is_err());
}

#[skuld::test]
fn a_step_the_table_does_not_know_is_an_error_not_an_empty_list() {
    assert!(missing("tset", "linux", env(&[])).is_err());
    assert!(missing(OPENSSH, "windows", env(&[])).is_err());
    assert!(missing("", "linux", env(&[])).is_err());
}

fn in_github_actions() -> Result<(), String> {
    match std::env::var("GITHUB_ACTIONS").as_deref() {
        Ok("true") => Ok(()),
        _ => Err("GITHUB_ACTIONS is not true; CI checks that its claims are made".to_owned()),
    }
}

/// Without this, a renamed claim variable, a renamed step or a changed runner condition turns a claim-gated test into
/// "ignored" on every leg with CI green. Labelled `vm` so that the vm job (`SKULD_LABELS=vm`) runs it too.
#[skuld::test(requires = [in_github_actions], labels = [crate::vm_gate::VM])]
fn this_runner_makes_the_claims_the_ci_step_owes() {
    let step = std::env::var(STEP_VAR)
        .unwrap_or_else(|_| panic!("{STEP_VAR} is not set; the step must name itself in .github/workflows/ci.yaml"));
    let absent = missing(&step, std::env::consts::OS, |name| std::env::var(name).ok())
        .unwrap_or_else(|why| panic!("{why}; check the step's env in .github/workflows/ci.yaml"));
    assert!(
        absent.is_empty(),
        "the {step} step on {} must set {}; check its env in .github/workflows/ci.yaml",
        std::env::consts::OS,
        absent
            .iter()
            .map(|claim| format!("{}={}", claim.var, claim.value))
            .collect::<Vec<_>>()
            .join(" and "),
    );
}
