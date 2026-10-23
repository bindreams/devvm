use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::ErrorKind;
use crate::dirs::Dirs;
use crate::test_support::fixtures::anna_home;
use skuld::env;

fn lookup<'a>(vars: &'a [(&'a str, OsString)]) -> impl Fn(&str) -> Option<OsString> + 'a {
    move |name| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    }
}

#[skuld::test]
fn xdg_overrides_are_honored_for_all_four(#[fixture(anna_home)] home: &Path) {
    let base = home.join("x y");
    let vars = [
        ("XDG_CONFIG_HOME", base.join("c").into_os_string()),
        ("XDG_DATA_HOME", base.join("d").into_os_string()),
        ("XDG_CACHE_HOME", base.join("ca").into_os_string()),
        ("XDG_STATE_HOME", base.join("s").into_os_string()),
    ];
    let dirs = Dirs::from_lookup(None, &lookup(&vars)).unwrap();
    assert_eq!(dirs.config, base.join("c").join("devvm"));
    assert_eq!(dirs.data, base.join("d").join("devvm"));
    assert_eq!(dirs.cache, base.join("ca").join("devvm"));
    assert_eq!(dirs.state, base.join("s").join("devvm"));
}

#[skuld::test]
fn defaults_are_linux_style_under_home(#[fixture(anna_home)] home: &Path) {
    let dirs = Dirs::from_lookup(Some(home.to_path_buf()), &lookup(&[])).unwrap();
    assert_eq!(dirs.config, home.join(".config").join("devvm"));
    assert_eq!(dirs.data, home.join(".local").join("share").join("devvm"));
    assert_eq!(dirs.cache, home.join(".cache").join("devvm"));
    assert_eq!(dirs.state, home.join(".local").join("state").join("devvm"));
}

#[skuld::test]
fn empty_or_relative_xdg_values_are_ignored(#[fixture(anna_home)] home: &Path) {
    let vars = [
        ("XDG_DATA_HOME", OsString::new()),
        ("XDG_CACHE_HOME", OsString::from("rel/dir")),
    ];
    let dirs = Dirs::from_lookup(Some(home.to_path_buf()), &lookup(&vars)).unwrap();
    assert_eq!(dirs.data, home.join(".local").join("share").join("devvm"));
    assert_eq!(dirs.cache, home.join(".cache").join("devvm"));
}

#[skuld::test]
fn no_home_and_no_xdg_is_an_error() {
    let error = Dirs::from_lookup(None, &lookup(&[])).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config);
    assert!(error.message().contains("XDG_CONFIG_HOME"), "{error}");
    assert!(
        error.message().contains("HOME") && error.message().contains("USERPROFILE"),
        "{error}"
    );
}

#[skuld::test]
fn no_home_is_fine_when_every_xdg_variable_is_set(#[fixture(anna_home)] home: &Path) {
    let vars = [
        ("XDG_CONFIG_HOME", home.join("c").into_os_string()),
        ("XDG_DATA_HOME", home.join("d").into_os_string()),
        ("XDG_CACHE_HOME", home.join("ca").into_os_string()),
        ("XDG_STATE_HOME", home.join("s").into_os_string()),
    ];
    assert!(Dirs::from_lookup(None, &lookup(&vars)).is_ok());
}

#[skuld::test]
fn under_places_all_four_below_the_root_and_names_subdirectories(#[fixture(anna_home)] home: &Path) {
    let dirs = Dirs::under(home);
    for dir in [&dirs.config, &dirs.data, &dirs.cache, &dirs.state] {
        assert!(dir.starts_with(home), "{}", dir.display());
        assert_eq!(dir.file_name().unwrap(), "devvm");
    }
    assert_eq!(dirs.layers(), dirs.data.join("layers"));
    assert_eq!(dirs.downloads(), dirs.cache.join("downloads"));
    assert_eq!(dirs.quarantine(), dirs.data.join("quarantine"));
    let distinct: std::collections::HashSet<PathBuf> = [&dirs.config, &dirs.data, &dirs.cache, &dirs.state]
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(distinct.len(), 4);
}

#[skuld::test]
fn from_env_reads_the_process_environment(#[fixture(env)] env: &skuld::EnvGuard, #[fixture(anna_home)] home: &Path) {
    let state = home.join("st");
    env.set("XDG_STATE_HOME", state.to_str().unwrap());
    let dirs = Dirs::from_env().unwrap();
    assert_eq!(dirs.state, state.join("devvm"));
}

#[skuld::test]
fn dirs_instance_paths_live_under_state(#[fixture(anna_home)] home: &Path) {
    let dirs = Dirs::under(home);
    assert_eq!(dirs.instances(), dirs.state.join("instances"));
    assert_eq!(dirs.instance_locks(), dirs.state.join("locks"));
    assert_eq!(dirs.instance_quarantine(), dirs.state.join("quarantine"));
    assert!(!dirs.state.exists(), "the accessors must not create anything");
}
