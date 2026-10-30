use std::collections::HashMap;

use super::{ACP, PYYAML, missing};

fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    let vars: HashMap<_, _> = vars.iter().copied().collect();
    move |name| vars.get(name).map(|value| (*value).to_owned())
}

fn names(claims: Vec<&super::Claim>) -> Vec<&'static str> {
    claims.into_iter().map(|claim| claim.var).collect()
}

#[skuld::test]
fn linux_must_claim_pyyaml() {
    assert_eq!(names(missing("linux", env(&[]))), [PYYAML.var]);
    assert!(missing("linux", env(&[(PYYAML.var, PYYAML.value)])).is_empty());
}

#[skuld::test]
fn windows_must_claim_the_code_page() {
    assert_eq!(names(missing("windows", env(&[]))), [ACP.var]);
    assert!(missing("windows", env(&[(ACP.var, ACP.value)])).is_empty());
}

#[skuld::test]
fn a_claim_counts_only_with_its_documented_value() {
    for wrong in ["", "0", "yes", "true", "1 ", "01", "1253"] {
        assert_eq!(
            names(missing("linux", env(&[(PYYAML.var, wrong)]))),
            [PYYAML.var],
            "{wrong:?}"
        );
        assert_eq!(
            names(missing("windows", env(&[(ACP.var, wrong)]))),
            [ACP.var],
            "{wrong:?}"
        );
    }
}

#[skuld::test]
fn a_runner_owes_no_claim_it_does_not_cover() {
    assert!(missing("macos", env(&[])).is_empty());
    assert!(missing("linux", env(&[(PYYAML.var, PYYAML.value)])).is_empty());
    assert!(!ACP.is_made_in(env(&[(PYYAML.var, PYYAML.value)])));
}

fn in_github_actions() -> Result<(), String> {
    match std::env::var("GITHUB_ACTIONS").as_deref() {
        Ok("true") => Ok(()),
        _ => Err("GITHUB_ACTIONS is not true; CI checks that its claims are made".to_owned()),
    }
}

/// Without this, a renamed claim variable or a changed runner condition turns a claim-gated test into "ignored" on
/// every leg with CI green.
#[skuld::test(requires = [in_github_actions])]
fn this_runner_makes_the_claims_the_ci_job_owes() {
    let absent = missing(std::env::consts::OS, |name| std::env::var(name).ok());
    assert!(
        absent.is_empty(),
        "the {} runner must set {}; check the Test step's env in .github/workflows/ci.yaml",
        std::env::consts::OS,
        absent
            .iter()
            .map(|claim| format!("{}={}", claim.var, claim.value))
            .collect::<Vec<_>>()
            .join(" and "),
    );
}
