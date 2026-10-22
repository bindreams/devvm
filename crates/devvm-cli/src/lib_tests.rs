use std::ffi::OsString;
use std::path::PathBuf;

use devvm::exit::{ExitStatus, ExitWidth};

use crate::{Deps, PANICKING, run_locked, status_file_hint};

fn args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

fn no_env(_: &str) -> Option<OsString> {
    None
}

#[skuld::test]
fn status_file_hint_reads_both_flag_forms() {
    assert_eq!(
        status_file_hint(&args(&["--status-file", "a.json", "build", "x"]), &no_env),
        Some(PathBuf::from("a.json"))
    );
    assert_eq!(
        status_file_hint(&args(&["build", "--status-file=b.json", "x"]), &no_env),
        Some(PathBuf::from("b.json"))
    );
}

#[skuld::test]
fn status_file_hint_falls_back_to_the_environment_and_the_flag_wins() {
    let env = |name: &str| (name == "DEVVM_STATUS_FILE").then(|| OsString::from("env.json"));
    assert_eq!(
        status_file_hint(&args(&["build", "x"]), &env),
        Some(PathBuf::from("env.json"))
    );
    assert_eq!(
        status_file_hint(&args(&["--status-file", "flag.json", "build"]), &env),
        Some(PathBuf::from("flag.json"))
    );
    assert_eq!(status_file_hint(&args(&["build", "x"]), &no_env), None);
}

#[skuld::test]
fn status_file_hint_ignores_flags_after_double_dash() {
    assert_eq!(
        status_file_hint(&args(&["run", "vm", "--", "--status-file", "x.json"]), &no_env),
        None
    );
    assert_eq!(
        status_file_hint(
            &args(&["--status-file", "a.json", "run", "--", "--status-file", "x.json"]),
            &no_env
        ),
        Some(PathBuf::from("a.json"))
    );
}

#[skuld::test]
fn status_file_hint_with_a_missing_value_is_none() {
    assert_eq!(status_file_hint(&args(&["build", "--status-file"]), &no_env), None);
}

#[skuld::test]
fn an_empty_status_file_value_is_the_same_as_none() {
    let env = |name: &str| (name == "DEVVM_STATUS_FILE").then(|| OsString::from("env.json"));
    let empty_env = |name: &str| (name == "DEVVM_STATUS_FILE").then(OsString::new);
    assert_eq!(status_file_hint(&args(&["build", "x"]), &empty_env), None);
    assert_eq!(status_file_hint(&args(&["--status-file=", "build"]), &no_env), None);
    assert_eq!(status_file_hint(&args(&["--status-file", "", "build"]), &no_env), None);
    assert_eq!(
        status_file_hint(&args(&["--status-file=", "build"]), &env),
        Some(PathBuf::from("env.json"))
    );
}

#[skuld::test]
fn a_double_dash_is_never_the_status_file_value() {
    assert_eq!(status_file_hint(&args(&["--status-file", "--", "run"]), &no_env), None);
    assert_eq!(status_file_hint(&args(&["--status-file", "--"]), &no_env), None);
}

/// A panic on another thread must not interleave with the normal finish: from `finish` until the process exits, the
/// panic hook's lock stays taken.
#[skuld::test]
fn the_normal_finish_holds_the_panic_hooks_lock_until_the_exit() {
    let exit = run_locked(ExitWidth::LowByte, args(&["build", "x"]), Deps::real(), &|_, _| {
        Ok(ExitStatus::Code(3))
    });
    assert_eq!(exit.code, 3);
    assert!(PANICKING.try_lock().is_err(), "a panic now would race the exit");
    drop(exit);
    assert!(PANICKING.try_lock().is_ok());
}
