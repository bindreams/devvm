use super::unsupported::UnsupportedHost;
use super::{Host, current, host_exit_width};
use crate::Arch;
use crate::exit::ExitWidth;
use crate::test_support::host::TestHost;

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
