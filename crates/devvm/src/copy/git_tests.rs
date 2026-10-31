use std::ffi::OsString;
use std::path::Path;

use super::{Git, adds_to_configuration, cleared_names};
use crate::copy::copy_tests::git_command;
use crate::test_support::fixtures::MemoryProgress;
use crate::test_support::host::TestHost;

fn names(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

fn cleared(list: &[&str]) -> Vec<String> {
    cleared_names(&TestHost, names(list))
        .into_iter()
        .map(|name| name.into_string().unwrap())
        .collect()
}

#[skuld::test]
fn the_variables_that_add_to_the_configuration_are_kept() {
    let kept = [
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
        "GIT_CONFIG_KEY_12",
        "GIT_CONFIG_VALUE_12",
    ];
    assert_eq!(cleared(&kept), Vec::<String>::new());
}

#[skuld::test]
fn every_other_git_variable_is_cleared_known_or_not() {
    let all = [
        "GIT_DIR",
        "GIT_TRACE",
        "GIT_TRACE2_EVENT",
        "GIT_CEILING_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_FUTURE_THING",
    ];
    assert_eq!(
        cleared(&all),
        [
            "GIT_CEILING_DIRECTORIES",
            "GIT_CONFIG",
            "GIT_DIR",
            "GIT_FUTURE_THING",
            "GIT_TRACE",
            "GIT_TRACE2_EVENT"
        ]
    );
}

#[skuld::test]
fn a_config_variable_whose_suffix_is_not_digits_is_cleared() {
    let odd = [
        "GIT_CONFIG_KEY_",
        "GIT_CONFIG_KEY_x",
        "GIT_CONFIG_KEY_1a",
        "GIT_CONFIG_VALUE_",
        "GIT_CONFIG_VALUE_-1",
        "GIT_CONFIG_COUNTS",
        "GIT_CONFIG_PARAMETERS2",
        "GIT_CONFIG_KEY",
    ];
    assert_eq!(cleared(&odd).len(), odd.len());
}

#[skuld::test]
fn other_variables_are_not_touched() {
    assert_eq!(
        cleared(&["PATH", "HOME", "GITHUB_TOKEN", "GIT", "NOT_GIT_DIR", "XGIT_DIR"]),
        Vec::<String>::new()
    );
}

#[skuld::test]
fn a_lowercase_name_is_judged_by_the_hosts_rule_for_names() {
    // Windows reads `git_dir` as `GIT_DIR`; elsewhere it is another variable. The config pairs are kept in any case.
    let list = ["git_dir", "Git_Trace", "git_config_key_0", "Git_Config_Parameters"];
    let expected: &[&str] = if cfg!(windows) { &["Git_Trace", "git_dir"] } else { &[] };
    assert_eq!(cleared(&list), expected);
}

#[cfg(windows)]
#[skuld::test]
fn a_name_is_classified_as_windows_folds_it_not_as_rust_does() {
    assert_eq!(cleared(&["G\u{131}T_DIR"]), Vec::<String>::new());
    assert_eq!(
        cleared(&["GIT_CONFIG_PARAMETER\u{17F}"]),
        ["GIT_CONFIG_PARAMETER\u{17F}"]
    );
}

#[skuld::test]
fn the_cleared_names_are_sorted_and_listed_once() {
    assert_eq!(
        cleared(&["GIT_TRACE", "GIT_DIR", "GIT_TRACE", "GIT_ALTERNATE_OBJECT_DIRECTORIES"]),
        ["GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_DIR", "GIT_TRACE"]
    );
}

#[skuld::test]
fn the_snapshot_says_which_variables_it_ignores_without_their_values() {
    let progress = MemoryProgress::default();
    let git = Git::from_names(&TestHost, names(&["PATH", "GIT_TRACE", "GIT_DIR"]), &progress);
    assert_eq!(
        progress.messages(),
        ["The following variables have been unset: GIT_DIR, GIT_TRACE"]
    );
    assert_eq!(git.cleared, names(&["GIT_DIR", "GIT_TRACE"]));
}

#[skuld::test]
fn the_snapshot_says_nothing_when_no_variable_is_ignored() {
    let progress = MemoryProgress::default();
    Git::from_names(
        &TestHost,
        names(&["PATH", "GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"]),
        &progress,
    );
    assert_eq!(progress.messages(), Vec::<String>::new());
}

#[skuld::test]
fn the_command_removes_exactly_the_cleared_variables() {
    let progress = MemoryProgress::default();
    let git = Git::from_names(
        &TestHost,
        names(&[
            "PATH",
            "GIT_DIR",
            "GIT_TRACE",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_KEY_0",
        ]),
        &progress,
    );
    let command = git.command(std::path::Path::new("."));
    let mut removed: Vec<_> = command
        .get_envs()
        .map(|(name, value)| {
            assert!(value.is_none(), "{name:?} is set, not removed");
            name.to_string_lossy().into_owned()
        })
        .collect();
    removed.sort();
    assert_eq!(removed, ["GIT_DIR", "GIT_TRACE"]);
}

#[skuld::test]
fn every_variable_git_calls_local_is_cleared_unless_it_adds_to_the_configuration() {
    let out = git_command(Path::new("."))
        .args(["rev-parse", "--local-env-vars"])
        .output()
        .expect("the tests need `git` on PATH");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reported = names(&String::from_utf8(out.stdout).unwrap().lines().collect::<Vec<_>>());
    assert!(!reported.is_empty(), "git reported no local variables");
    let cleared = cleared_names(&TestHost, reported.clone());
    for name in reported.iter().filter(|name| !cleared.contains(name)) {
        assert!(
            adds_to_configuration(&TestHost, name),
            "git calls {name:?} local, and devvm leaves it set"
        );
    }
}
