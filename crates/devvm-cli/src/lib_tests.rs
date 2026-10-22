use std::ffi::OsString;
use std::path::PathBuf;

use crate::status_file_hint;

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
