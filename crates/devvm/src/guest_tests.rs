use secrecy::SecretString;

use crate::ErrorKind;
use crate::GuestKind;
use crate::guest::fake::FakeGuests;
use crate::guest::{BuiltinGuests, Guests, SeedKeys};
use crate::recipe::Shell;

#[skuld::test]
fn builtin_guests_name_the_missing_guest() {
    for (kind, name) in [
        (GuestKind::Linux, "linux"),
        (GuestKind::Windows, "windows"),
        (GuestKind::Macos, "macos"),
    ] {
        let os = BuiltinGuests.os(kind).err().expect("no guest is built in yet");
        assert_eq!(os.kind(), ErrorKind::NotSupported);
        assert!(os.message().contains(name), "{}", os.message());
        let builder = BuiltinGuests.builder(kind).err().expect("no guest is built in yet");
        assert_eq!(builder.kind(), ErrorKind::NotSupported);
        assert!(builder.message().contains(name), "{}", builder.message());
    }
}

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
