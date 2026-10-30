use std::error::Error as _;

use crate::error::{Error, ErrorKind};

#[skuld::test]
fn kind_strings_are_the_wire_names() {
    use ErrorKind::*;
    let table = [
        (Usage, "usage"),
        (NotFound, "not-found"),
        (AlreadyExists, "already-exists"),
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
        (NoExitStatus, "no-exit-status"),
        (ConnectionLost, "connection-lost"),
        (HostKey, "host-key"),
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

#[skuld::test]
fn a_cleanup_that_worked_leaves_the_error_as_it_was() {
    let error = Error::new(ErrorKind::Io, "cannot move the copy into place")
        .after_cleanup("the temporary file", std::io::Result::Ok(()));
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(error.message(), "cannot move the copy into place");
}

#[skuld::test]
fn a_cleanup_that_failed_is_named_with_what_it_left_behind() {
    let cause = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "the file is locked");
    let error =
        Error::new(ErrorKind::DiskFull, "cannot write the copy").after_cleanup("the temporary file /x/y", Err(cause));
    assert_eq!(error.kind(), ErrorKind::DiskFull, "the first failure stays the error");
    let text = error.message();
    assert!(text.starts_with("cannot write the copy"), "{text}");
    assert!(text.contains("/x/y") && text.contains("the file is locked"), "{text}");
}

#[skuld::test]
fn a_full_disk_write_is_disk_full_and_carries_the_callers_hint() {
    for kind in [std::io::ErrorKind::StorageFull, std::io::ErrorKind::QuotaExceeded] {
        let cause = std::io::Error::new(kind, "the device is full");
        let error = Error::write(std::path::Path::new("out/x.part"), cause, "free space, then resume");
        assert_eq!(error.kind(), ErrorKind::DiskFull, "{kind:?}");
        assert!(error.message().contains("out/x.part"), "{error}");
        assert!(error.message().ends_with("; free space, then resume"), "{error}");
        assert!(error.source().is_some());
    }
}

#[skuld::test]
fn another_failed_write_is_io_naming_the_path() {
    let cause = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
    let error = Error::write(std::path::Path::new("out/x.part"), cause, "free space, then resume");
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(error.message(), "cannot write out/x.part: access denied");
}
