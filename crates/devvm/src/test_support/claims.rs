//! The claims a CI job makes, by environment variable, so a test that only runs where it is claimed cannot go missing
//! unnoticed.

#[path = "claims_tests.rs"]
mod claims_tests;

/// A variable a CI job sets to claim that its runs cover something. Only the documented `value` counts.
pub struct Claim {
    pub var: &'static str,
    pub value: &'static str,
}

/// This runner's ANSI code page is 1252 (`host_tests.rs`).
pub const ACP: Claim = Claim {
    var: "DEVVM_COVERS_ACP",
    value: "1252",
};

/// This runner reads the seed with PyYAML (`guest/linux/seed_pyyaml_tests.rs`).
pub const PYYAML: Claim = Claim {
    var: "DEVVM_COVERS_PYYAML",
    value: "1",
};

/// The claims each runner OS (`std::env::consts::OS`) must make. The workflow's Test step sets them by `runner.os`.
const OWED: &[(&str, &[&Claim])] = &[("linux", &[&PYYAML]), ("windows", &[&ACP])];

impl Claim {
    /// Whether this process's environment makes the claim.
    pub fn is_made(&self) -> bool {
        self.is_made_in(|name| std::env::var(name).ok())
    }

    pub fn is_made_in(&self, env: impl Fn(&str) -> Option<String>) -> bool {
        env(self.var).as_deref() == Some(self.value)
    }
}

/// The claims runner `os` owes that `env` does not make.
pub fn missing(os: &str, env: impl Fn(&str) -> Option<String>) -> Vec<&'static Claim> {
    OWED.iter()
        .filter(|(owner, _)| *owner == os)
        .flat_map(|(_, claims)| claims.iter().copied())
        .filter(|claim| !claim.is_made_in(&env))
        .collect()
}
