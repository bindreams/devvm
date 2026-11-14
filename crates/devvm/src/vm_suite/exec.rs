//! The exec suite through the library: `Vm::run` in a real Ubuntu guest. The cases are the table of `exec/cases.rs`,
//! which `devvm-cli/tests/vm_cli.rs` runs through the `devvm` executable.

use super::fixture::{SharedGuest, shared_guest};
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
