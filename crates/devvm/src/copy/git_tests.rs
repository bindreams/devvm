use std::ffi::OsString;

use super::{Git, cleared_names};
use crate::test_support::fixtures::MemoryProgress;
use crate::test_support::host::TestHost;

fn names(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

fn cleared(list: &[&str], ignore_case: bool) -> Vec<String> {
    cleared_names(names(list), ignore_case)
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
    assert_eq!(cleared(&kept, false), Vec::<String>::new());
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
        cleared(&all, false),
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
    assert_eq!(cleared(&odd, false).len(), odd.len());
}

#[skuld::test]
fn other_variables_are_not_touched() {
    assert_eq!(
        cleared(
            &["PATH", "HOME", "GITHUB_TOKEN", "GIT", "NOT_GIT_DIR", "XGIT_DIR"],
            true
        ),
        Vec::<String>::new()
    );
}

#[skuld::test]
fn a_lowercase_name_is_left_where_names_are_case_sensitive() {
    assert_eq!(
        cleared(&["git_dir", "Git_Trace", "git_config_key_0"], false),
        Vec::<String>::new()
    );
}

#[skuld::test]
fn a_lowercase_name_is_the_same_variable_where_names_ignore_case() {
    // Windows reads `git_dir` as `GIT_DIR`. The config pairs are kept in any case.
    assert_eq!(
        cleared(
            &["git_dir", "Git_Trace", "git_config_key_0", "Git_Config_Parameters"],
            true
        ),
        ["Git_Trace", "git_dir"]
    );
}

#[skuld::test]
fn the_cleared_names_are_sorted_and_listed_once() {
    assert_eq!(
        cleared(
            &["GIT_TRACE", "GIT_DIR", "GIT_TRACE", "GIT_ALTERNATE_OBJECT_DIRECTORIES"],
            false
        ),
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
fn the_variables_of_this_process_are_read_for_each_snapshot() {
    // The real environment carries whatever the runner set: the note and the removals agree, and none is a kept one.
    let progress = MemoryProgress::default();
    let git = Git::from_process(&TestHost, &progress);
    assert_eq!(progress.messages().len(), usize::from(!git.cleared.is_empty()));
    for name in &git.cleared {
        let name = name.to_string_lossy().to_ascii_uppercase();
        assert!(name.starts_with("GIT_"), "{name}");
        assert!(
            !name.starts_with("GIT_CONFIG_KEY_") && !name.starts_with("GIT_CONFIG_VALUE_"),
            "{name}"
        );
    }
}
