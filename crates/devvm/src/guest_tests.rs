use crate::ErrorKind;
use crate::GuestKind;
use crate::guest::fake::FakeGuests;
use crate::guest::{BuiltinGuests, Guests};
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
