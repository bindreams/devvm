use std::error::Error as _;
use std::path::Path;

use crate::error::{Error, ErrorKind, tail_of};
use crate::test_support::fixtures::unicode_home;

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
        (ProcessRecord, "process-record"),
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
fn with_context_names_what_failed_and_keeps_the_source_but_not_the_error() {
    let inner = Error::new(ErrorKind::Internal, "cannot build it").with_source(std::io::Error::other("the cause"));
    let error = inner.with_context(ErrorKind::Download, "cannot fetch https://h/x");
    assert_eq!(error.kind(), ErrorKind::Download);
    assert_eq!(error.message(), "cannot fetch https://h/x: cannot build it");
    assert_eq!(error.source().unwrap().to_string(), "the cause");
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

#[skuld::test]
fn with_tail_ends_the_message_with_the_last_20_lines_of_the_text() {
    let text: Vec<String> = (1..=25).map(|n| format!("line {n}")).collect();
    let error = Error::new(ErrorKind::Recipe, "step 1 failed").with_tail("the step's output", &text.join("\r\n"));
    assert_eq!(error.kind(), ErrorKind::Recipe);
    let tail: Vec<String> = (6..=25).map(|n| format!("line {n}")).collect();
    assert_eq!(
        error.message(),
        format!("step 1 failed\nthe end of the step's output:\n{}", tail.join("\n"))
    );
}

#[skuld::test]
fn with_tail_of_empty_text_adds_nothing_and_keeps_the_source() {
    let source = std::io::Error::other("cause");
    let error = Error::io(ErrorKind::Io, "cannot", source).with_tail("a log", "");
    assert_eq!(error.message(), "cannot: cause");
    let error = error.with_tail("a log", "x");
    assert!(error.source().is_some(), "the source stays");
}

fn failed() -> Error {
    Error::new(ErrorKind::Unavailable, "the guest finished early")
}

#[skuld::test]
fn with_log_tail_appends_the_last_lines_of_a_file(#[fixture(unicode_home)] home: &Path) {
    let log = home.join("serial.log");
    let text: Vec<String> = (1..=45).map(|n| format!("line {n}")).collect();
    // A Windows guest's file ends its lines with CRLF; the last one is unterminated.
    std::fs::write(&log, text.join("\r\n")).unwrap();
    let error = failed()
        .with_source(std::io::Error::other("cause"))
        .with_log_tail("the serial log", &log);
    let tail: Vec<String> = (26..=45).map(|n| format!("line {n}")).collect();
    assert_eq!(
        error.message(),
        format!(
            "the guest finished early\nthe end of the serial log:\n{}",
            tail.join("\n")
        )
    );
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert!(error.source().is_some(), "the source stays");
}

#[skuld::test]
fn with_log_tail_of_a_file_with_one_huge_unterminated_line_keeps_the_end(#[fixture(unicode_home)] home: &Path) {
    let log = home.join("serial.log");
    let mut bytes = b"first\n".to_vec();
    bytes.extend(std::iter::repeat_n(b'x', 5 << 20));
    bytes.extend_from_slice(b" the end");
    std::fs::write(&log, bytes).unwrap();
    let error = failed().with_log_tail("the serial log", &log);
    let message = error.message();
    assert!(
        message.starts_with("the guest finished early\nthe end of the serial log:\nfirst\n"),
        "{}",
        message.len()
    );
    assert!(message.ends_with("xxx the end"), "{}", message.len());
    assert!(
        message.len() < 64 << 10,
        "the huge line is cut: {} bytes",
        message.len()
    );
}

/// A reader whose first fill is interrupted, as a read can be by a signal.
struct InterruptedOnce {
    inner: std::io::Cursor<Vec<u8>>,
    interrupted: bool,
}

impl std::io::Read for InterruptedOnce {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl std::io::BufRead for InterruptedOnce {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if !self.interrupted {
            self.interrupted = true;
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        self.inner.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.inner.consume(amount);
    }
}

#[skuld::test]
fn an_interrupted_read_of_a_log_is_retried() {
    let reader = InterruptedOnce {
        inner: std::io::Cursor::new(b"one\ntwo\n".to_vec()),
        interrupted: false,
    };
    assert_eq!(tail_of(reader).unwrap(), "one\ntwo");
}

#[skuld::test]
fn with_log_tail_adds_nothing_for_a_missing_file(#[fixture(unicode_home)] home: &Path) {
    let error = failed().with_log_tail("the serial log", &home.join("absent.log"));
    assert_eq!(error.message(), "the guest finished early");
    let empty = home.join("empty.log");
    std::fs::write(&empty, "").unwrap();
    let error = failed().with_log_tail("the serial log", &empty);
    assert_eq!(error.message(), "the guest finished early");
}

#[skuld::test]
fn with_log_tail_says_so_when_the_file_cannot_be_read(#[fixture(unicode_home)] home: &Path) {
    // A directory is not a file that can be read, on any host.
    let error = failed().with_log_tail("the serial log", home);
    let message = error.message();
    assert!(
        message.starts_with("the guest finished early\nthe end of the serial log:\n(cannot be read: "),
        "{message}"
    );
}
