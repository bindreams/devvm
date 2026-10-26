//! Re-exec helpers: tests that need a separate process run their own test binary in helper mode.
//!
//! The test binary's `main` calls [`run_if_requested`] before `skuld::run_all()`. A test builds the child with
//! [`command`], which spawns the same binary with [`HELPER_VAR`] set to the helper's name.

use std::process::Command;

/// Environment variable that selects helper mode and names the helper.
pub const HELPER_VAR: &str = "DEVVM_TEST_HELPER";

/// A helper's entry point; receives the arguments given to [`command`].
pub type HelperFn = fn(&[String]);

/// Runs the helper named by [`HELPER_VAR`] from `helpers`, then exits the process. Returns when the variable is unset.
///
/// # Panics
/// If the variable names a helper that is not in `helpers`.
pub fn run_if_requested(helpers: &[(&str, HelperFn)]) {
    let Some(name) = std::env::var_os(HELPER_VAR) else {
        return;
    };
    let name = name.to_string_lossy();
    let Some((_, helper)) = helpers.iter().find(|(known, _)| *known == name) else {
        panic!("unknown {HELPER_VAR} `{name}`");
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    helper(&args);
    std::process::exit(0);
}

/// A command that re-executes the current test binary as helper `name` with `args`.
pub fn command(name: &str, args: &[&str]) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("test binary path should be known"));
    command.env(HELPER_VAR, name).args(args);
    command
}

/// `command` for cosca: a frozen spawn needs cosca's own command type.
pub fn cosca_command(name: &str, args: &[&str]) -> cosca::Command {
    let exe = std::env::current_exe().expect("test binary path should be known");
    let mut command = cosca::Command::new();
    command.arg(exe).args(args).env(HELPER_VAR, name);
    command
}
