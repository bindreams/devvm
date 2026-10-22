use std::error::Error as _;

use crate::error::{Error, ErrorKind};

#[skuld::test]
fn kind_strings_are_the_wire_names() {
    use ErrorKind::*;
    let table = [
        (Usage, "usage"),
        (NotFound, "not-found"),
        (InvalidName, "invalid-name"),
        (Recipe, "recipe"),
        (Config, "config"),
        (Io, "io"),
        (Download, "download"),
        (DiskFull, "disk-full"),
        (Checksum, "checksum"),
        (ExpiredBase, "expired-base"),
        (Store, "store"),
        (StatusFile, "status-file"),
        (DetachRefused, "detach-refused"),
        (NoExitStatus, "no-exit-status"),
        (ConnectionLost, "connection-lost"),
        (Unavailable, "unavailable"),
        (NotSupported, "not-supported"),
        (Panic, "panic"),
        (Internal, "internal"),
    ];
    for (kind, name) in table {
        assert_eq!(kind.as_str(), name);
    }
}

#[skuld::test]
fn io_error_message_names_context_and_cause() {
    let cause = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
    let error = Error::io(ErrorKind::Io, "cannot read /x/y", cause);
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(error.message(), "cannot read /x/y: access denied");
    assert_eq!(error.to_string(), "io: cannot read /x/y: access denied");
    assert!(error.source().is_some());
}

#[skuld::test]
fn with_source_keeps_the_message_and_exposes_the_source() {
    let error = Error::new(ErrorKind::Config, "bad").with_source(std::fmt::Error);
    assert_eq!(error.message(), "bad");
    assert!(error.source().is_some());
    assert!(Error::new(ErrorKind::Config, "bad").source().is_none());
}

#[skuld::test]
fn error_is_send_sync_static() {
    fn assert_bounds<T: Send + Sync + 'static>() {}
    assert_bounds::<Error>();
}
