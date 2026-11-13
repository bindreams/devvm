use secrecy::SecretString;

use crate::ErrorKind;
use crate::GuestKind;
use crate::guest::fake::FakeGuests;
use crate::guest::{Guests, SeedKeys};
use crate::recipe::Shell;

#[skuld::test]
fn a_linux_guest_wraps_a_script_for_its_shell_and_refuses_cmd() {
    let os = FakeGuests.os(GuestKind::Linux).unwrap();
    let argv = |shell, script| os.shell_argv(shell, script).unwrap();
    assert_eq!(argv(None, "echo hi"), ["bash", "-c", "echo hi"]);
    assert_eq!(argv(Some(Shell::Bash), "echo hi"), ["bash", "-c", "echo hi"]);
    assert_eq!(argv(Some(Shell::Sh), "echo hi"), ["sh", "-c", "echo hi"]);
    assert_eq!(
        argv(Some(Shell::Pwsh), "Write-Output hi"),
        ["pwsh", "-NoProfile", "-Command", "Write-Output hi"]
    );
    let error = os.shell_argv(Some(Shell::Cmd), "dir").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("cmd"), "{}", error.message());
}

#[skuld::test]
fn user_path_undoes_sftp_path() {
    let drive = FakeGuests.os_on_drive(GuestKind::Windows, "Z:");
    let root = std::path::Path::new("/srv/jail");
    let rooted = FakeGuests.os_rooted_at(GuestKind::Linux, root);
    for guest in [&drive, &rooted, &FakeGuests.os(GuestKind::Linux).unwrap()] {
        let typed: &[&str] = if std::ptr::eq(guest, &drive) {
            &["Z:/", "Z:/a", "Z:/a/b.txt"]
        } else {
            &["/", "/a", "/a/b.txt"]
        };
        for typed in typed {
            let sent = guest.sftp_path(typed, "/home");
            assert_eq!(guest.user_path(&sent), *typed, "{typed} was sent as {sent}");
        }
    }
    // A path the server reports from outside the root stays as it is.
    assert_eq!(rooted.user_path("/srv/jailbreak/a"), "/srv/jailbreak/a");
}

#[skuld::test]
fn debug_output_of_the_seed_keys_does_not_hold_the_private_key() {
    let keys = SeedKeys {
        client_public: "ssh-ed25519 CLIENT-PUBLIC".to_owned(),
        host_private: SecretString::from("HOST-PRIVATE-KEY-TEXT"),
        host_public: "ssh-ed25519 HOST-PUBLIC".to_owned(),
    };
    let shown = format!("{keys:?}");
    assert!(!shown.contains("HOST-PRIVATE-KEY-TEXT"), "{shown}");
    assert!(
        shown.contains("CLIENT-PUBLIC") && shown.contains("HOST-PUBLIC"),
        "{shown}"
    );
}

#[skuld::test]
fn the_fake_windows_guests_setting_is_the_real_ones_script_carried_by_a_shell_the_test_server_runs() {
    use crate::RunAs;
    use crate::recipe::{Setting, UacMode};
    let command = FakeGuests
        .os(GuestKind::Windows)
        .unwrap()
        .setting_command(&Setting::Uac(UacMode::AutoApprove))
        .unwrap();
    assert_eq!(command.run_as, RunAs::Elevated);
    assert_eq!(command.argv[..2], ["sh", "-c"]);
    assert!(
        command.argv[2].contains("ConsentPromptBehaviorAdmin"),
        "{:?}",
        command.argv
    );
}
