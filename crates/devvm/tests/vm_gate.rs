//! Pins the VM-test gate: the opt-out is exact, and a missing VM dependency fails a test unless opted out.

#[path = "support/vm_gate.rs"]
mod vm_gate;

use std::process::Command;

use vm_gate::gate;

/// When set, this test binary is the probe's child: the probe fixture really fails.
const CHILD_VAR: &str = "DEVVM_VM_GATE_CHILD";
/// The probe's own opt-out switch, so the parent's `DEVVM_SKIP_VM_TESTS` never affects it.
const PROBE_SKIP_VAR: &str = "DEVVM_VM_GATE_PROBE_SKIP";

fn main() {
    skuld::run_all();
}

#[skuld::test]
fn gate_skips_only_for_exactly_one() {
    for value in [
        None,
        Some("0"),
        Some(""),
        Some("yes"),
        Some("true"),
        Some("1 "),
        Some("01"),
    ] {
        assert_eq!(gate(value), Ok(()), "{value:?}");
    }
    assert_eq!(gate(Some("1")), Err("DEVVM_SKIP_VM_TESTS=1".to_owned()));
}

// The probe ===========================================================================================================

fn probe_enabled() -> Result<(), String> {
    gate(std::env::var(PROBE_SKIP_VAR).ok().as_deref()).map_err(|_| format!("{PROBE_SKIP_VAR}=1"))
}

/// Stands in for a VM host whose QEMU is missing. It fails only in the child, so the parent run stays green.
#[skuld::fixture(requires = [probe_enabled])]
fn probe_host() -> Result<(), String> {
    if std::env::var_os(CHILD_VAR).is_some() {
        Err("QEMU not found".to_owned())
    } else {
        Ok(())
    }
}

#[skuld::test]
fn probe_boots(#[fixture(probe_host)] _host: &()) {}

fn run_probe(skip: Option<&str>) -> (bool, String) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .env(CHILD_VAR, "1")
        .env_remove(PROBE_SKIP_VAR)
        .arg("probe_boots");
    if let Some(value) = skip {
        command.env(PROBE_SKIP_VAR, value);
    }
    let output = command.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

#[skuld::test]
fn a_vm_fixture_setup_error_fails_unless_opted_out() {
    let (ok, text) = run_probe(None);
    assert!(!ok, "a missing VM dependency must fail the test, not skip it:\n{text}");
    assert!(text.contains("QEMU not found"), "{text}");
    assert!(text.contains("1 failed"), "the probe ran and failed:\n{text}");

    let (ok, text) = run_probe(Some("1"));
    assert!(ok, "the explicit opt-out ignores the test:\n{text}");
    assert!(
        text.contains("1 ignored") && text.contains(&format!("{PROBE_SKIP_VAR}=1")),
        "{text}"
    );

    let (ok, text) = run_probe(Some("0"));
    assert!(
        !ok && text.contains("QEMU not found"),
        "any other value keeps the test on:\n{text}"
    );
}

#[skuld::test]
fn vm_tests_enabled_reads_the_environment() {
    let expected = gate(std::env::var(vm_gate::SKIP_VAR).ok().as_deref());
    assert_eq!(vm_gate::vm_tests_enabled(), expected);
}
