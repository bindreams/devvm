use std::path::Path;

use super::unsupported::UnsupportedHost;
use super::{Host, current, host_exit_width};
use crate::Arch;
use crate::exit::ExitWidth;
use crate::test_support::file_mode;
use crate::test_support::fixtures::anna_home;
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

#[skuld::test]
fn unsupported_host_detach_changes_nothing() {
    let mut cmd = cosca::Command::new();
    UnsupportedHost.detach(&mut cmd).unwrap();
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
fn windows_host_detach_accepts_a_command() {
    let mut cmd = cosca::Command::new();
    super::windows::WindowsHost.detach(&mut cmd).unwrap();
}

#[skuld::test]
fn test_host_accelerates_with_tcg_and_detaches_like_the_current_host() {
    let host = TestHost;
    assert!(matches!(host.accelerator(Arch::host()), Ok(super::Accel::Tcg)));
    assert_eq!(host.exit_width(), current().exit_width());
    let mut cmd = cosca::Command::new();
    host.detach(&mut cmd).unwrap();
}

#[cfg(windows)]
#[skuld::test]
fn quiet_sets_create_no_window() {
    let mut cmd = helper::command("console-window", &[]);
    current().quiet(&mut cmd);
    let output = cmd.output().unwrap();
    assert!(output.status.success());
    // A console of its own (only this process is on it) that has no window. Without the flag the child shares its
    // parent's console, or has none.
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
fn write_private_is_create_new_and_owner_only(#[fixture(anna_home)] home: &Path) {
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
fn executable_bit_round_trips_on_unix_and_is_inert_on_windows(#[fixture(anna_home)] home: &Path) {
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
