use std::path::Path;

use super::other::OtherHost;
use super::{Comparison, FileId, HoldOutcome, Host, current, host_exit_width};
use crate::Arch;
use crate::exit::ExitWidth;
use crate::test_groups::{Group, PINNED_CHILD, acp_1252, symlinks};
use crate::test_support::file_mode;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::helper;
use crate::test_support::host::TestHost;
use crate::test_support::other_account::a_directory_of_another_account;
use crate::test_support::symlink::symlink;
#[cfg(windows)]
use skuld::env;

#[skuld::test]
fn unsupported_host_exit_width_is_low_byte() {
    assert_eq!(OtherHost.exit_width(), ExitWidth::LowByte);
}

#[skuld::test]
fn unsupported_host_names_itself_in_unavailable() {
    let missing = OtherHost.accelerator(Arch::host()).unwrap_err().missing;
    assert!(missing.contains(std::env::consts::OS), "{missing}");
    assert!(missing.contains("0.1.0"), "{missing}");
}

/// What the `console-window` helper says about its own console: whether it has a window, and whether it has a console
/// at all. Not how many processes share the console: other tests start and end processes on it meanwhile.
#[derive(Debug, PartialEq, Eq)]
struct ConsoleFacts {
    window: String,
    attached: bool,
}

/// The helper's own console facts when started from `cmd`.
fn console_facts(mut cmd: cosca::Command) -> ConsoleFacts {
    let output = cmd.output().expect("the helper should run");
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let field = |name: &str| {
        text.split(' ')
            .find_map(|part| part.strip_prefix(name))
            .unwrap_or_else(|| panic!("the helper printed no {name}: {text}"))
            .to_owned()
    };
    ConsoleFacts {
        window: field("window="),
        attached: field("console=") != "0",
    }
}

fn console_window_command() -> cosca::Command {
    helper::cosca_command("console-window", &[])
}

/// A child that was not detached, started with `CREATE_NO_WINDOW`: on Windows it has a console of its own with no
/// window, whether or not this process has a console. Without the flag, a parent with no console would give it a new
/// console with a window.
fn undetached_control() -> ConsoleFacts {
    let mut cmd = console_window_command();
    cmd.no_window();
    let facts = console_facts(cmd);
    assert!(
        facts.attached,
        "a child that is not detached should have a console: {facts:?}"
    );
    assert_ne!(facts.window, "window", "the control should have no window: {facts:?}");
    facts
}

#[skuld::test]
fn unsupported_host_detach_changes_nothing() {
    let mut cmd = console_window_command();
    cmd.no_window();
    OtherHost.detach(&mut cmd).unwrap();
    assert_eq!(console_facts(cmd), undetached_control());
}

#[skuld::test]
fn current_host_exit_width_matches_the_os() {
    let expected = if cfg!(windows) {
        ExitWidth::Full32
    } else {
        ExitWidth::LowByte
    };
    assert_eq!(host_exit_width(), expected);
    assert_eq!(current().exit_width(), expected);
}

#[cfg(windows)]
#[skuld::test]
fn windows_host_exit_width_is_full32() {
    assert_eq!(super::windows::WindowsHost.exit_width(), ExitWidth::Full32);
}

#[cfg(windows)]
#[skuld::test]
fn windows_host_detach_leaves_the_child_without_a_console() {
    // With `no_window`, a detach that stopped detaching gives the child a windowless console, never a window.
    let mut cmd = console_window_command();
    cmd.no_window();
    super::windows::WindowsHost.detach(&mut cmd).unwrap();

    let detached = console_facts(cmd);
    undetached_control();
    assert!(!detached.attached, "a detached child has no console: {detached:?}");
}

#[skuld::test]
fn test_host_accelerates_with_tcg_and_detaches_like_the_current_host() {
    let host = TestHost;
    assert!(matches!(host.accelerator(Arch::host()), Ok(super::Accel::Tcg)));
    assert_eq!(host.exit_width(), current().exit_width());
    let (mut ours, mut current_hosts) = (console_window_command(), console_window_command());
    ours.no_window();
    current_hosts.no_window();
    host.detach(&mut ours).unwrap();
    current().detach(&mut current_hosts).unwrap();
    assert_eq!(console_facts(ours), console_facts(current_hosts));
}

#[cfg(windows)]
#[skuld::test]
fn quiet_sets_create_no_window() {
    let mut cmd = helper::command("console-window", &[]);
    current().quiet(&mut cmd);
    let output = cmd.output().unwrap();
    assert!(output.status.success());
    // A console of its own (only this process is on it) that has no window. Without the flag the child shares its
    // parent's console, or gets a new one with a window when the parent has none.
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "window=none console=1");
}

#[cfg(not(windows))]
#[skuld::test]
fn quiet_leaves_a_command_runnable() {
    let mut cmd = helper::command("console-window", &[]);
    current().quiet(&mut cmd);
    let output = cmd.output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "window=n/a console=n/a");
}

#[skuld::test]
fn unsupported_host_has_no_system_dirs() {
    assert!(OtherHost.system_dirs().is_none());
}

#[cfg(windows)]
#[skuld::test]
fn windows_host_system_dirs_come_from_the_os_and_not_the_environment(#[fixture(env)] env: &skuld::EnvGuard) {
    for name in ["SystemRoot", "SYSTEMROOT", "windir", "WINDIR"] {
        env.set(name, "Z:/not/windows");
    }
    let dirs = super::windows::WindowsHost
        .system_dirs()
        .expect("Windows has system directories");
    assert!(dirs.root.is_dir(), "{}", dirs.root.display());
    assert!(dirs.system32.is_dir(), "{}", dirs.system32.display());
    assert!(dirs.system32.starts_with(&dirs.root));
    assert!(dirs.system32.join("kernel32.dll").is_file());
    assert_ne!(dirs.root, Path::new("Z:/not/windows"));
}

#[skuld::test]
fn write_private_is_create_new_and_owner_only(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("private key");
    current().write_private(&path, b"secret").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"secret");
    assert_eq!(file_mode::mode(&path), cfg!(unix).then_some(0o600));

    let error = current().write_private(&path, b"other").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"secret",
        "an existing file is left alone"
    );
}

#[skuld::test]
fn set_mtime_sets_the_time_of_a_directory_and_of_a_read_only_file(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("dir");
    std::fs::create_dir(&dir).unwrap();
    let file = home.join("read-only.txt");
    std::fs::write(&file, "x").unwrap();
    let mut permissions = std::fs::metadata(&file).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&file, permissions).unwrap();
    for path in [&dir, &file] {
        current().set_mtime(path, 1_000_000_000).unwrap();
        let since = std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        assert_eq!(since.as_secs(), 1_000_000_000, "{}", path.display());
    }
    // Removable again, whatever the platform makes of a read-only file.
    #[allow(
        clippy::permissions_set_readonly_false,
        reason = "a temporary file in the test's own directory"
    )]
    {
        let mut permissions = std::fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(&file, permissions).unwrap();
    }
}

#[skuld::test]
fn executable_bit_round_trips_on_unix_and_is_inert_on_windows(#[fixture(unicode_home)] home: &Path) {
    let tool = home.join("tool");
    std::fs::write(&tool, b"#!/bin/sh").unwrap();
    file_mode::set_mode(&tool, 0o644);
    assert!(!current().is_executable(&tool).unwrap());

    current().set_executable(&tool).unwrap();
    assert_eq!(current().is_executable(&tool).unwrap(), cfg!(unix));
    assert_eq!(file_mode::mode(&tool), cfg!(unix).then_some(0o755));

    // Execute is added where read is set, and nowhere else.
    let owner_only = home.join("owner only");
    std::fs::write(&owner_only, b"x").unwrap();
    file_mode::set_mode(&owner_only, 0o600);
    current().set_executable(&owner_only).unwrap();
    assert_eq!(file_mode::mode(&owner_only), cfg!(unix).then_some(0o700));

    let write_only = home.join("write only");
    std::fs::write(&write_only, b"x").unwrap();
    file_mode::set_mode(&write_only, 0o200);
    current().set_executable(&write_only).unwrap();
    assert_eq!(file_mode::mode(&write_only), cfg!(unix).then_some(0o200));
}

#[skuld::test]
fn a_failed_cleanup_of_a_partial_file_is_named_next_to_the_write_error(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("key");
    let write_error = std::io::Error::new(std::io::ErrorKind::StorageFull, "the disk is full");

    let error = super::after_failed_write(&path, write_error, || {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the file is locked",
        ))
    });

    assert_eq!(
        error.kind(),
        std::io::ErrorKind::StorageFull,
        "the write error stays the error"
    );
    let text = error.to_string();
    assert!(text.contains("the disk is full"), "{text}");
    assert!(
        text.contains("key") && text.contains("the file is locked"),
        "the partial file and why it stays are not named: {text}"
    );
}

#[skuld::test]
fn a_cleaned_up_partial_file_leaves_the_write_error_as_it_was(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("key");
    std::fs::write(&path, b"half").unwrap();

    let error = super::after_failed_write(&path, std::io::Error::other("the disk is full"), || {
        std::fs::remove_file(&path)
    });

    assert_eq!(error.to_string(), "the disk is full");
    assert!(!path.exists());
}

#[cfg(windows)]
#[skuld::test]
fn a_second_failure_while_replacing_a_file_is_named_next_to_the_first() {
    let first = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
    let second = std::io::Error::other("the attribute is locked");
    let error = super::windows::also_failed(first, "clearing the read-only attribute", second);
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied,
        "the first failure stays the kind"
    );
    let text = error.to_string();
    for part in [
        "access denied",
        "clearing the read-only attribute",
        "the attribute is locked",
    ] {
        assert!(text.contains(part), "{part} is missing: {text}");
    }
}

// Replacing a file ====================================================================================================

/// What a copy meets in a destination tree: a symlink to a directory where the guest has a file or a file symlink.
#[skuld::test]
fn replace_file_replaces_a_symlink_to_a_directory_and_leaves_the_directory(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(symlinks)] _group: &Group,
) {
    let real = home.join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("held"), "kept").unwrap();
    let link_to_file = home.join("target.txt");
    std::fs::write(&link_to_file, "pointed at").unwrap();
    for (n, as_link) in [false, true].into_iter().enumerate() {
        let (from, to) = (home.join(format!("from{n}")), home.join(format!("to{n}")));
        symlink(&real, &to);
        if as_link {
            symlink(&link_to_file, &from);
        } else {
            std::fs::write(&from, "new").unwrap();
        }
        current().replace_file(&from, &to).unwrap();
        let meta = std::fs::symlink_metadata(&to).unwrap();
        assert_eq!(meta.file_type().is_symlink(), as_link, "link source: {as_link}");
        assert!(!std::fs::metadata(&to).unwrap().is_dir(), "link source: {as_link}");
        assert!(!from.exists(), "link source: {as_link}");
        assert_eq!(std::fs::read_to_string(real.join("held")).unwrap(), "kept");
    }
}

// Publishing a directory ==============================================================================================

/// Runs `check` on the host this build runs on and on the host that only the tests of the shared rules use.
fn on_both_hosts(check: impl Fn(&dyn Host)) {
    check(current());
    check(&OtherHost);
}

/// A directory `name` below `home` holding the file `held`.
fn dir_with_a_file(home: &Path, name: &str) -> std::path::PathBuf {
    let dir = home.join(name);
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("held"), name).unwrap();
    dir
}

#[skuld::test]
fn publish_dir_moves_a_directory_to_a_name_nobody_has(#[fixture(unicode_home)] home: &Path) {
    on_both_hosts(|host| {
        let (from, to) = (dir_with_a_file(home, "from"), home.join("to"));
        host.publish_dir(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(std::fs::read_to_string(to.join("held")).unwrap(), "from");
        std::fs::remove_dir_all(&to).unwrap();
    });
}

#[skuld::test]
fn publish_dir_never_replaces_what_is_there_not_even_an_empty_directory(#[fixture(unicode_home)] home: &Path) {
    on_both_hosts(|host| {
        let from = dir_with_a_file(home, "from");
        let empty = home.join("empty");
        std::fs::create_dir(&empty).unwrap();
        let file = home.join("file");
        std::fs::write(&file, "mine").unwrap();
        for taken in [&empty, &file] {
            let error = host.publish_dir(&from, taken).unwrap_err();
            assert_eq!(
                error.kind(),
                std::io::ErrorKind::AlreadyExists,
                "{}: {error}",
                taken.display()
            );
        }
        assert!(
            empty.read_dir().unwrap().next().is_none(),
            "the empty directory is still empty"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "mine");
        assert_eq!(
            std::fs::read_to_string(from.join("held")).unwrap(),
            "from",
            "the source is untouched"
        );
        std::fs::remove_dir_all(&from).unwrap();
        std::fs::remove_dir_all(&empty).unwrap();
        std::fs::remove_file(&file).unwrap();
    });
}

#[skuld::test]
fn sync_dir_flushes_a_directory_that_exists(#[fixture(unicode_home)] home: &Path) {
    on_both_hosts(|host| host.sync_dir(home).unwrap());
}

/// Where a directory is flushed through a handle (Unix), a directory that is not there is an error, so the call is not
/// a stub that answers `Ok`. NTFS journals its directories and flushes nothing.
#[skuld::test]
fn sync_dir_of_a_missing_directory_fails_where_directories_are_flushed(#[fixture(unicode_home)] home: &Path) {
    let missing = home.join("missing");
    let error = current().sync_dir(&missing);
    match cfg!(unix) {
        true => assert_eq!(error.unwrap_err().kind(), std::io::ErrorKind::NotFound),
        false => error.unwrap(),
    }
}

// The ANSI code page ==================================================================================================

/// The locale whose ANSI code page is 1252 on every host, whatever the host's own locale.
const PINNED_LOCALE: &str = "en-US";

const GROUP_TEST: &str = "the_host_code_page_is_1252";

/// Needs the ANSI code page to be 1252: a host whose own is, and the child the pinning test starts. Fails on any other
/// host unless the group is off. Repeats the group's precondition, so a group that passed without the code page being
/// 1252 fails here.
#[skuld::test]
fn the_host_code_page_is_1252(#[fixture(acp_1252)] _group: &Group) {
    assert_eq!(current().ansi_code_page(), Some(1252));
}

#[skuld::test]
fn ansi_code_page_is_some_on_windows_and_none_elsewhere() {
    assert_eq!(current().ansi_code_page().is_some(), cfg!(windows));
    assert_eq!(OtherHost.ansi_code_page(), None);
}

/// A manifest that gives the process the ANSI code page of a locale. Windows reads `<exe>.manifest` for an executable
/// that has no embedded manifest, which the lib test binary does not.
fn pinning_manifest(locale: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <activeCodePage xmlns="http://schemas.microsoft.com/SMI/2019/WindowsSettings">{locale}</activeCodePage>
    </windowsSettings>
  </application>
</assembly>
"#
    )
}

fn can_pin_code_page_1252() -> Result<(), String> {
    if std::env::var_os(PINNED_CHILD).is_some() {
        return Err(format!("{PINNED_CHILD} is set: this is the pinned run"));
    }
    match current().ansi_code_page() {
        None => Err("this host has no ANSI code page".to_owned()),
        Some(1252) => Err("this host's ANSI code page is already 1252".to_owned()),
        Some(_) => Ok(()),
    }
}

/// Whether the libtest report in `stdout` lists `test` as passed (`test <name> ... ok`, padded between the words).
fn reports_passed(stdout: &str, test: &str) -> bool {
    stdout.lines().any(|line| {
        let words: Vec<_> = line.split_whitespace().collect();
        words == ["test", test, "...", "ok"]
    })
}

/// Covers 1252 on a host whose own code page is another: runs the whole lib suite in a copy of this binary whose
/// manifest pins `en-US`. Requires the child's report to list the group test as passed, so a pin that did not take
/// fails.
#[skuld::test(requires = [can_pin_code_page_1252])]
fn the_lib_suite_passes_in_a_process_pinned_to_acp_1252() {
    let dir = skuld::TempDir::new().unwrap_or_else(|e| panic!("cannot create a temp dir for the pinned binary: {e}"));
    let exe = dir.join("devvm-lib-acp-1252.exe");
    let this_exe = std::env::current_exe().unwrap_or_else(|e| panic!("cannot find the path of this test binary: {e}"));
    std::fs::copy(&this_exe, &exe)
        .unwrap_or_else(|e| panic!("cannot copy {} to {}: {e}", this_exe.display(), exe.display()));
    let manifest = dir.join("devvm-lib-acp-1252.exe.manifest");
    std::fs::write(&manifest, pinning_manifest(PINNED_LOCALE))
        .unwrap_or_else(|e| panic!("cannot write the manifest {}: {e}", manifest.display()));

    let mut cmd = std::process::Command::new(&exe);
    cmd.env("DEVVM_TEST_VM", "0")
        .env("DEVVM_TEST_ACP_1252", "1")
        .env(PINNED_CHILD, PINNED_LOCALE)
        .env_remove("SKULD_LABELS")
        .env_remove(helper::HELPER_VAR)
        .arg("--color=never");
    current().quiet(&mut cmd);
    let output = cmd.output().unwrap_or_else(|e| {
        panic!(
            "cannot start {} with activeCodePage={PINNED_LOCALE}: {e}",
            exe.display()
        )
    });
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "the lib suite failed at ANSI code page 1252 (manifest activeCodePage={PINNED_LOCALE}; {}):
{stdout}
{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        reports_passed(&stdout, GROUP_TEST),
        "the pinned run (manifest activeCodePage={PINNED_LOCALE}) exited 0 but its report does not list {GROUP_TEST} \
         as passed, so nothing shows it ran at 1252:\n{stdout}",
    );
}

#[skuld::test]
fn only_windows_compares_environment_variable_names_without_case() {
    let ignores_case = cfg!(windows);
    for host in [current(), &TestHost as &dyn Host] {
        assert_eq!(
            host.env_name_strip_prefix("git_dir".as_ref(), "GIT_").is_some(),
            ignores_case
        );
        assert_eq!(
            host.env_name_strip_prefix("GIT_DIR".as_ref(), "GIT_").as_deref(),
            Some("DIR")
        );
    }
    assert_eq!(OtherHost.env_name_strip_prefix("git_dir".as_ref(), "GIT_"), None);
    assert_eq!(
        OtherHost.env_name_strip_prefix("GIT_".as_ref(), "GIT_").as_deref(),
        Some("")
    );
    assert_eq!(OtherHost.env_name_strip_prefix("GIT".as_ref(), "GIT_"), None);
}

#[skuld::test]
fn an_unsupported_host_puts_the_runtime_directory_under_tmpdir_per_user() {
    let tmp = std::env::temp_dir();
    let var = |name: &str| (name == "TMPDIR").then(|| tmp.clone().into_os_string());
    let dir = OtherHost.run_dir_default(&var).unwrap();
    assert_eq!(dir, tmp.join("devvm-run-0"));
}

// The runtime directory -----------------------------------------------------------------------------------------------

#[skuld::test]
fn create_runtime_dir_makes_the_directory_and_accepts_it_again(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("run");
    current().create_runtime_dir(&dir).unwrap();
    assert!(dir.is_dir());
    current().create_runtime_dir(&dir).unwrap();
}

#[skuld::test]
fn create_runtime_dir_makes_one_level_and_a_missing_parent_is_not_found(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("missing").join("run");
    let error = current().create_runtime_dir(&dir).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
    assert!(!home.join("missing").exists());
}

#[skuld::test]
fn a_file_at_the_runtime_dir_path_is_refused(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("run");
    std::fs::write(&path, b"mine").unwrap();
    let error = current().create_runtime_dir(&path).unwrap_err();
    assert!(error.to_string().contains("not a directory"), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), b"mine");
}

/// A directory the test user does not own: `C:\ProgramData` is owned by the system, `/` by root. Nothing is written
/// into it.
#[skuld::test]
fn a_directory_owned_by_someone_else_is_refused_naming_the_owner() {
    let system = a_directory_of_another_account();
    let error = current().create_runtime_dir(&system).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied, "{error}");
    let text = error.to_string();
    assert!(text.contains(&system.display().to_string()), "{text}");
    assert!(text.contains("is owned by") && text.contains("not by you"), "{text}");
}

/// The check `create_runtime_dir` applies to a directory that exists, without making one.
#[skuld::test]
fn accept_runtime_dir_makes_nothing_and_a_missing_directory_is_not_found(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("run");
    let error = current().accept_runtime_dir(&dir).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
    assert!(!dir.exists());
}

#[skuld::test]
fn accept_runtime_dir_accepts_the_directory_create_runtime_dir_made(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("run");
    current().create_runtime_dir(&dir).unwrap();
    current().accept_runtime_dir(&dir).unwrap();
}

#[skuld::test]
fn accept_runtime_dir_refuses_a_file(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("run");
    std::fs::write(&path, b"mine").unwrap();
    let error = current().accept_runtime_dir(&path).unwrap_err();
    assert!(error.to_string().contains("not a directory"), "{error}");
}

#[skuld::test]
fn accept_runtime_dir_refuses_a_directory_owned_by_someone_else_naming_the_owner() {
    let system = a_directory_of_another_account();
    let error = current().accept_runtime_dir(&system).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied, "{error}");
    let text = error.to_string();
    assert!(text.contains(&system.display().to_string()), "{text}");
    assert!(text.contains("is owned by") && text.contains("not by you"), "{text}");
}

// File identity -------------------------------------------------------------------------------------------------------

#[skuld::test]
fn file_id_is_equal_for_one_file_and_differs_between_two_that_exist(#[fixture(unicode_home)] home: &Path) {
    let (a, b) = (home.join("a"), home.join("b"));
    std::fs::write(&a, b"").unwrap();
    std::fs::write(&b, b"").unwrap();
    let first = current().file_id(&a).unwrap();
    assert_eq!(first, current().file_id(&a).unwrap());
    assert_ne!(first, current().file_id(&b).unwrap());
}

#[skuld::test]
fn file_id_of_a_missing_file_is_not_found(#[fixture(unicode_home)] home: &Path) {
    let error = current().file_id(&home.join("gone")).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
}

#[skuld::test]
fn file_id_of_an_open_file_is_the_id_of_its_name(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("a");
    std::fs::write(&path, b"").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    assert_eq!(
        current().file_id_of_open(&file).unwrap(),
        current().file_id(&path).unwrap()
    );
}

#[skuld::test]
fn file_id_of_an_open_file_stays_when_its_name_leads_to_another_file(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("a");
    std::fs::write(&path, b"").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    let before = current().file_id_of_open(&file).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"").unwrap();
    assert_eq!(current().file_id_of_open(&file).unwrap(), before);
    assert_ne!(current().file_id(&path).unwrap(), before);
}

#[skuld::test]
fn comparing_a_file_id_with_a_look_at_a_name_says_same_different_or_absent() {
    let id = FileId { volume: 1, file: 2 };
    let other = FileId { volume: 1, file: 3 };
    assert_eq!(id.compare(Ok(id)).unwrap(), Comparison::Same);
    assert_eq!(id.compare(Ok(other)).unwrap(), Comparison::Different);
    for kind in [
        std::io::ErrorKind::NotFound,
        std::io::ErrorKind::NotADirectory,
        std::io::ErrorKind::InvalidFilename,
    ] {
        assert_eq!(id.compare(Err(kind.into())).unwrap(), Comparison::Absent, "{kind:?}");
    }
    let error = id
        .compare(Err(std::io::ErrorKind::PermissionDenied.into()))
        .expect_err("a look that failed is no answer");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}

#[skuld::test]
fn a_file_id_reads_back_from_its_text() {
    let id = FileId {
        volume: 0xdead_beef_0000_0001,
        file: (1u128 << 100) | 7,
    };
    let json = serde_json::to_string(&id).unwrap();
    assert!(json.starts_with('"'), "JSON has no 128-bit number: {json}");
    assert_eq!(serde_json::from_str::<FileId>(&json).unwrap(), id);
    assert!(serde_json::from_str::<FileId>("\"nonsense\"").is_err());
}

// Holds ---------------------------------------------------------------------------------------------------------------

fn taken(outcome: std::io::Result<HoldOutcome>) -> Box<dyn super::Hold> {
    match outcome.unwrap() {
        HoldOutcome::Taken(hold) => hold,
        other => panic!("expected to take the hold, got {other:?}"),
    }
}

#[skuld::test]
fn a_new_hold_creates_the_file_and_nobody_else_takes_it(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.hold");
    let hold = current().hold_new(&path).unwrap();
    assert!(path.exists());
    assert!(matches!(current().hold_existing(&path).unwrap(), HoldOutcome::Busy));
    let error = current().hold_new(&path).expect_err("the name is taken");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
    drop(hold);
    assert!(path.exists(), "dropping a hold leaves the file");
}

#[skuld::test]
fn a_hold_is_free_once_its_holder_is_gone(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.hold");
    drop(current().hold_new(&path).unwrap());
    let hold = taken(current().hold_existing(&path));
    assert!(matches!(current().hold_existing(&path).unwrap(), HoldOutcome::Busy));
    drop(hold);
    drop(taken(current().hold_existing(&path)));
}

#[skuld::test]
fn a_missing_file_is_no_hold(#[fixture(unicode_home)] home: &Path) {
    assert!(matches!(
        current().hold_existing(&home.join("gone")).unwrap(),
        HoldOutcome::Absent
    ));
    assert!(
        !home.join("gone").exists(),
        "a hold on an existing file creates nothing"
    );
}

/// What a child that inherited the hold is: another handle to the same open file, which keeps the hold when the
/// holder closes its own.
#[skuld::test]
fn a_duplicate_of_the_held_file_keeps_the_hold_when_the_holder_is_gone(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.hold");
    let hold = current().hold_new(&path).unwrap();
    let inherited = hold.file().try_clone().unwrap();
    drop(hold);
    assert!(matches!(current().hold_existing(&path).unwrap(), HoldOutcome::Busy));
    drop(inherited);
    drop(taken(current().hold_existing(&path)));
}

#[skuld::test]
fn deleting_a_hold_removes_the_file_and_frees_the_name(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.hold");
    current().hold_new(&path).unwrap().delete().unwrap();
    assert!(!path.exists());
    drop(current().hold_new(&path).unwrap());
}

/// What a child that inherited the hold is: another handle to the file, which outlives the delete. The name is free at
/// once all the same.
#[skuld::test]
fn a_deleted_hold_frees_its_name_while_a_duplicate_of_the_file_is_open(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.hold");
    let hold = current().hold_new(&path).unwrap();
    let duplicate = hold.file().try_clone().unwrap();

    hold.delete().unwrap();

    drop(current().hold_new(&path).expect("the name is free"));
    drop(duplicate);
}

/// The identity of a held file is read without the data access a hold refuses to anyone else.
#[skuld::test]
fn the_identity_of_a_held_file_can_be_read(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.hold");
    let hold = current().hold_new(&path).unwrap();
    let id = current().file_id(&path).unwrap();
    drop(hold);
    assert_eq!(current().file_id(&path).unwrap(), id);
}

/// The identity comes from the open file, so it is the same file as the name gives while the name is the file's.
#[skuld::test]
fn a_hold_knows_the_identity_of_its_file(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.hold");
    let hold = current().hold_new(&path).unwrap();
    assert_eq!(hold.id().unwrap(), current().file_id(&path).unwrap());
    drop(hold);
    let again = taken(current().hold_existing(&path));
    assert_eq!(again.id().unwrap(), current().file_id(&path).unwrap());
}

#[skuld::test]
fn a_hold_new_that_finds_the_name_taken_leaves_the_taken_file_and_makes_nothing_else(
    #[fixture(unicode_home)] home: &Path,
) {
    let dir = home.join("d");
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("x.hold");
    let hold = current().hold_new(&path).unwrap();

    let error = current().hold_new(&path).expect_err("the name is taken");

    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["x.hold"]);
    assert_eq!(current().file_id(&path).unwrap(), hold.id().unwrap());
}

// The hold of the hosts that have no Windows: run through `OtherHost`, which has the same hold, everywhere.

/// The name a launch draws is made by linking a file that is held already, from a private name; a file at the private
/// name is another launch's claim in the making, so the name is drawn again.
#[skuld::test]
fn a_hold_new_makes_its_file_under_a_private_name_first(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.sock.hold");
    std::fs::write(home.join("x.sock.hold.new"), b"").unwrap();

    let error = OtherHost.hold_new(&path).expect_err("the private name is taken");

    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
    assert!(!path.exists());
}

#[skuld::test]
fn a_hold_new_leaves_no_private_name_behind(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("d");
    std::fs::create_dir(&dir).unwrap();
    let hold = OtherHost.hold_new(&dir.join("x.sock.hold")).unwrap();
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["x.sock.hold"]);
    assert!(matches!(
        OtherHost.hold_existing(&dir.join("x.sock.hold")).unwrap(),
        HoldOutcome::Busy
    ));
    drop(hold);
}

/// A sweep and a record's cleanup can race for one claim: the loser finds the file gone.
#[skuld::test]
fn deleting_a_hold_whose_file_is_already_unlinked_is_fine(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.hold");
    let hold = OtherHost.hold_new(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    hold.delete().unwrap();
}

/// A name that is a link is not opened through, on every host: what it leads to is somebody else's file.
#[skuld::test]
fn holding_an_existing_name_that_is_a_symlink_opens_nothing_through_it_on_every_host(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(symlinks)] _group: &Group,
) {
    let victim = home.join("victim");
    std::fs::write(&victim, b"theirs").unwrap();
    let link = home.join("x.sock.hold");
    symlink(&victim, &link);
    on_both_hosts(|host| {
        let error = host.hold_existing(&link).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error}");
        assert!(error.to_string().contains("is a link"), "{error}");
    });
    std::fs::File::open(&victim)
        .unwrap()
        .try_lock()
        .expect("nobody holds the victim");
}
