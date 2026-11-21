//! The exec suite through the library: `Vm::run` in a real Ubuntu guest. The cases are the table of `exec/cases.rs`,
//! which `devvm-cli/tests/vm_cli.rs` runs through the `devvm` executable.

use super::fixture::{SharedGuest, run_ok, run_ok_as, shared_guest};
use super::waits::{answer_to, named};
use crate::{Command, ExitStatus, Guest, RunAs, Vm};
use cases::{Case, Ended, ExecTarget, Observed, Os};

pub(super) mod cases;

/// Runs a case with `Vm::run` in a guest of `os`.
pub(super) struct InGuest<'a>(pub &'a Vm, pub Os);

impl ExecTarget for InGuest<'_> {
    fn os(&self) -> Os {
        self.1
    }

    fn run(&self, case: &Case) -> Observed {
        let command = Command {
            argv: case.argv.clone(),
            run_as: if case.elevated { RunAs::Elevated } else { RunAs::User },
            cwd: case.cwd.clone(),
        };
        let _watch = answer_to(&named(&case.argv));
        let output = self
            .0
            .run(&command)
            .unwrap_or_else(|e| panic!("devvm could not run {:?}: {e}", case.argv));
        if let Some(loss) = &output.loss {
            panic!("{:?} ended with a loss: {loss}", case.argv);
        }
        Observed {
            ended: match output.status {
                ExitStatus::Code(code) => Ended::Code(code),
                ExitStatus::Signal { name, .. } => Ended::Signal(name),
            },
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

#[skuld::test]
fn the_guest_is_named_after_the_instance_id(#[fixture(shared_guest)] guest: &SharedGuest) {
    let want = guest.vm.instance().dir().id().hostname();
    let named = run_ok(&guest.vm, &["hostname"]);
    assert_eq!(named.trim(), want);
    assert_ne!(named.trim(), guest.vm.name(), "the label is not the hostname");
}

/// The low privilege flags the guest kernel logged at boot (`Hyper-V: privilege flags low 0x..., high 0x...`): CPUID
/// leaf 0x40000003, EAX. `None` when the kernel logged no such line.
fn hyperv_privilege_flags_low(guest: &SharedGuest) -> Option<u32> {
    let log = run_ok_as(
        &guest.vm,
        RunAs::Elevated,
        &["sh", "-c", "dmesg | grep 'Hyper-V: privilege flags' || true"],
    );
    let (_, after) = log.split_once("privilege flags low 0x")?;
    let digits: String = after.chars().take_while(char::is_ascii_hexdigit).collect();
    Some(u32::from_str_radix(&digits, 16).unwrap_or_else(|e| panic!("no hex after \"low 0x\" in {log:?}: {e}")))
}

/// Without the hypercall MSRs (bit 5) the guest loses its Hyper-V clock, and can panic at boot.
#[skuld::test]
fn the_guest_is_granted_the_hyperv_hypercall_msrs(#[fixture(shared_guest)] guest: &SharedGuest) {
    let low = hyperv_privilege_flags_low(guest).expect("the guest kernel logged no Hyper-V privilege flags at boot");
    assert_ne!(
        low & 0x20,
        0,
        "the privilege flags are {low:#x}: the hypercall MSRs (bit 5) are absent"
    );
}

#[skuld::test]
fn exit_7_125_255(#[fixture(shared_guest)] guest: &SharedGuest) {
    cases::exit_7_125_255(&InGuest(&guest.vm, Os::Linux));
}

#[skuld::test]
fn a_guest_printing_devvm_error_and_exiting_125_is_a_guest_status(#[fixture(shared_guest)] guest: &SharedGuest) {
    cases::a_guest_printing_devvm_error_and_exiting_125_is_a_guest_status(&InGuest(&guest.vm, Os::Linux));
}

#[skuld::test]
fn kill_minus_segv_dollar_dollar_exits_139(#[fixture(shared_guest)] guest: &SharedGuest) {
    cases::kill_minus_segv_dollar_dollar_exits_139(&InGuest(&guest.vm, Os::Linux));
}

#[skuld::test]
fn argv_round_trips_hostile_arguments(#[fixture(shared_guest)] guest: &SharedGuest) {
    cases::argv_round_trips_hostile_arguments(&InGuest(&guest.vm, Os::Linux));
}

#[skuld::test]
fn cwd_is_honored(#[fixture(shared_guest)] guest: &SharedGuest) {
    cases::cwd_is_honored(&InGuest(&guest.vm, Os::Linux));
}

#[skuld::test]
fn elevated_cwd_is_honored(#[fixture(shared_guest)] guest: &SharedGuest) {
    cases::elevated_cwd_is_honored(&InGuest(&guest.vm, Os::Linux));
}

#[skuld::test]
fn elevated_runs_as_root(#[fixture(shared_guest)] guest: &SharedGuest) {
    cases::elevated_runs_as_root(&InGuest(&guest.vm, Os::Linux));
}
