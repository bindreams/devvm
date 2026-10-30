use std::path::Path;

use super::unsupported::UnsupportedHost;
use super::{Host, current, host_exit_width};
use crate::Arch;
use crate::exit::ExitWidth;
use crate::test_support::file_mode;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::helper;
use crate::test_support::host::TestHost;
#[cfg(windows)]
use skuld::env;

#[skuld::test]
fn unsupported_host_exit_width_is_low_byte() {
    assert_eq!(UnsupportedHost.exit_width(), ExitWidth::LowByte);
}

#[skuld::test]
fn unsupported_host_names_itself_in_unavailable() {
    let missing = UnsupportedHost.accelerator(Arch::host()).unwrap_err().missing;
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
    UnsupportedHost.detach(&mut cmd).unwrap();
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
    assert!(UnsupportedHost.system_dirs().is_none());
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

// The ANSI code page ==================================================================================================

/// The variable a CI job sets to claim that its runs cover this ANSI code page.
const COVERS_ACP: &str = "DEVVM_COVERS_ACP";

/// Set in the pinned child to the locale its manifest asks for, so the child does not start a child of its own and its
/// claim test knows the pin is what it is checking.
const PINNED_CHILD: &str = "DEVVM_PINNED_CHILD";

/// The locale whose ANSI code page is 1252 on every host, whatever the host's own locale.
const PINNED_LOCALE: &str = "en-US";

const CLAIM_TEST: &str = "the_host_code_page_is_the_one_this_run_claims";

/// Runs the claim test only where a job claims a code page. Without a claim the test is ignored and the reason says
/// which code page this host has and where the others are covered, so a local run never reads as covering 1252.
fn code_page_claimed() -> Result<(), String> {
    match std::env::var(COVERS_ACP) {
        Ok(claimed) if !claimed.is_empty() => Ok(()),
        _ => Err(format!(
            "no code page claimed; this host's ACP is {}; 1252 is covered by CI (Test job on windows-latest)",
            current()
                .ansi_code_page()
                .map_or_else(|| "not applicable".to_owned(), |acp| acp.to_string()),
        )),
    }
}

#[skuld::test(requires = [code_page_claimed])]
fn the_host_code_page_is_the_one_this_run_claims() {
    let claimed = std::env::var(COVERS_ACP).unwrap();
    let actual = current().ansi_code_page();
    let why = match std::env::var(PINNED_CHILD) {
        Ok(locale) => {
            format!("the manifest's activeCodePage={locale} did not pin the process to ANSI code page {claimed}")
        }
        Err(_) => {
            "this host's code page differs from the claim; if CI set the claim, its runner image changed".to_owned()
        }
    };
    assert_eq!(
        actual.map(|acp| acp.to_string()),
        Some(claimed.clone()),
        "{COVERS_ACP}={claimed} but this process's ANSI code page is {actual:?}: {why}",
    );
}

#[skuld::test]
fn ansi_code_page_is_some_on_windows_and_none_elsewhere() {
    assert_eq!(current().ansi_code_page().is_some(), cfg!(windows));
    assert_eq!(UnsupportedHost.ansi_code_page(), None);
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

/// Covers 1252 on a host whose own code page is another (this PC's is UTF-8): the whole lib suite runs again in a copy
/// of this binary whose manifest asks for the code page of `en-US`. The child also runs the claim test, and this test
/// requires the child's report to list it as passed, so a pin that did not take fails the run.
#[skuld::test(requires = [can_pin_code_page_1252])]
fn the_lib_suite_passes_in_a_process_pinned_to_acp_1252() {
    let dir = skuld::TempDir::new().unwrap();
    let exe = dir.join("devvm-lib-acp-1252.exe");
    std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    std::fs::write(
        dir.join("devvm-lib-acp-1252.exe.manifest"),
        pinning_manifest(PINNED_LOCALE),
    )
    .unwrap();

    let mut cmd = std::process::Command::new(&exe);
    cmd.env("DEVVM_SKIP_VM_TESTS", "1")
        .env(COVERS_ACP, "1252")
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
        reports_passed(&stdout, CLAIM_TEST),
        "the pinned run (manifest activeCodePage={PINNED_LOCALE}) exited 0 but its report does not list {CLAIM_TEST} \
         as passed, so nothing shows it ran at 1252:\n{stdout}",
    );
}
