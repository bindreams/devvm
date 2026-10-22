use crate::ErrorKind;
use crate::GuestKind;
use crate::guest::{BuiltinGuests, Guests};

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
