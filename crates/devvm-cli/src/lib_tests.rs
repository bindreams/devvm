use std::ffi::OsString;
use std::path::PathBuf;

use devvm::exit::{ExitStatus, ExitWidth};

use devvm::{Error, ErrorKind};

use crate::{Deps, PANICKING, flushed, run_locked, status_file_hint};

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

/// A writer whose flush fails.
struct FlushRefused;

impl std::io::Write for FlushRefused {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::ErrorKind::BrokenPipe.into())
    }
}

#[skuld::test]
fn a_success_whose_output_cannot_be_flushed_is_an_io_error() {
    let error = flushed(Ok(ExitStatus::Code(0)), &mut FlushRefused).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
}

#[skuld::test]
fn a_failure_stands_when_the_flush_fails_too() {
    let outcome = Err(Error::new(ErrorKind::Usage, "bad"));
    assert_eq!(
        flushed(outcome, &mut FlushRefused).unwrap_err().kind(),
        ErrorKind::Usage
    );
}

#[skuld::test]
fn a_flushed_success_keeps_its_status() {
    assert_eq!(
        flushed(Ok(ExitStatus::Code(7)), &mut Vec::new()).unwrap(),
        ExitStatus::Code(7)
    );
}
