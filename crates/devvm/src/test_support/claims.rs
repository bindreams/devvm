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

/// This runner creates files on NTFS to measure what it stores (`host/names_tests.rs`).
pub const NTFS_NAMES: Claim = Claim {
    var: "DEVVM_COVERS_NTFS_NAMES",
    value: "1",
};

/// This run's SFTP server is OpenSSH's own `sftp-server` (`access/sftp_tests.rs`).
pub const OPENSSH_SFTP: Claim = Claim {
    var: "DEVVM_COVERS_OPENSSH_SFTP",
    value: "1",
};

/// This job runs the VM tests: the workflow turns them off for every other job (`tests/support/vm_gate.rs`).
pub const VM_TESTS: Claim = Claim {
    var: "DEVVM_SKIP_VM_TESTS",
    value: "0",
};

/// The variable each workflow step or job that runs the tests sets to its name in [`OWED`]. They differ in what they
/// claim, so the table cannot be keyed by the runner OS alone.
pub const STEP_VAR: &str = "DEVVM_CI_STEP";

/// The claims each workflow step or job (`ci.yaml`) must make on each runner OS (`std::env::consts::OS`). A pair not
/// listed is not a step or job of the workflow.
const OWED: &[(&str, &str, &[&Claim])] = &[
    ("test", "linux", &[&PYYAML]),
    ("test", "macos", &[]),
    ("test", "windows", &[&ACP, &NTFS_NAMES]),
    ("openssh-sftp", "linux", &[&OPENSSH_SFTP, &PYYAML]),
    ("vm", "windows", &[&ACP, &VM_TESTS]),
];

impl Claim {
    /// Whether this process's environment makes the claim.
    pub fn is_made(&self) -> bool {
        self.is_made_in(|name| std::env::var(name).ok())
    }

    pub fn is_made_in(&self, env: impl Fn(&str) -> Option<String>) -> bool {
        env(self.var).as_deref() == Some(self.value)
    }
}

/// The claims the step `step` on runner `os` owes that `env` does not make. A pair the table does not list is an error:
/// an unnamed or renamed step must not read as owing nothing.
pub fn missing(step: &str, os: &str, env: impl Fn(&str) -> Option<String>) -> Result<Vec<&'static Claim>, String> {
    let (_, _, claims) = OWED
        .iter()
        .find(|(owner, runner, _)| *owner == step && *runner == os)
        .ok_or_else(|| format!("the step {step:?} on {os} owes no listed claims (test_support/claims.rs)"))?;
    Ok(claims.iter().copied().filter(|claim| !claim.is_made_in(&env)).collect())
}
