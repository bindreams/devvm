use std::error::Error as _;

use crate::error::{self, Error, ErrorKind};

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

/// A marker error, as `access::session` puts on an operation that had not started.
#[derive(Debug)]
struct Marker;

impl std::fmt::Display for Marker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("marker")
    }
}

impl std::error::Error for Marker {}

fn is_marked(error: &Error) -> bool {
    error.source().is_some_and(|source| source.is::<Marker>())
}

#[skuld::test]
fn an_error_with_a_serial_log_shows_the_path_in_parentheses_after_its_body() {
    let error = Error::new(ErrorKind::ConnectionLost, "the VM process exited").with_serial_log("/x/serial.log");
    assert_eq!(error.message(), "the VM process exited (serial log: /x/serial.log)");
    assert_eq!(
        error.to_string(),
        "connection-lost: the VM process exited (serial log: /x/serial.log)"
    );
    assert_eq!(error.serial_log(), Some(std::path::Path::new("/x/serial.log")));
    assert_eq!(Error::new(ErrorKind::Io, "no log").serial_log(), None);
}

#[skuld::test]
fn the_path_comes_before_the_tail_and_the_tail_stays_in_the_body() {
    let error = Error::new(ErrorKind::ConnectionLost, "lost")
        .with_serial_log("/x/serial.log")
        .with_tail("the log", "a\nb");
    assert_eq!(
        error.message(),
        "lost (serial log: /x/serial.log)\nthe end of the log:\na\nb"
    );
    assert_eq!(
        error.to_string(),
        "connection-lost: lost (serial log: /x/serial.log)\nthe end of the log:\na\nb"
    );
    assert_eq!(error.body(), "lost\nthe end of the log:\na\nb");
    let tail_first = Error::new(ErrorKind::Io, "cannot")
        .with_tail("the log", "a")
        .with_serial_log("/x/serial.log");
    assert_eq!(
        tail_first.message(),
        "cannot (serial log: /x/serial.log)\nthe end of the log:\na",
        "the order the parts are set in does not matter"
    );
}

#[skuld::test]
fn a_step_or_context_added_after_a_tail_lands_after_the_sentence_not_after_the_tail() {
    let tailed = || {
        Error::new(ErrorKind::ConnectionLost, "lost")
            .with_serial_log("/x/serial.log")
            .with_tail("the log", "a\nb")
    };
    let tail = "\nthe end of the log:\na\nb";
    let step = tailed().with_step("try again");
    assert_eq!(
        step.message(),
        format!("lost; try again (serial log: /x/serial.log){tail}")
    );
    let context = tailed().with_context(ErrorKind::Io, "reading");
    assert_eq!(
        context.message(),
        format!("reading: lost (serial log: /x/serial.log){tail}")
    );
    let twice = tailed().with_tail("another log", "c");
    assert_eq!(
        twice.message(),
        format!("lost (serial log: /x/serial.log){tail}\nthe end of another log:\nc")
    );
}

#[skuld::test]
fn a_body_with_a_newline_keeps_the_path_after_its_last_line_of_body_and_before_the_tail() {
    let error = Error::new(ErrorKind::Io, "first\nsecond")
        .with_serial_log("/x/serial.log")
        .with_tail("the log", "a");
    assert_eq!(
        error.message(),
        "first\nsecond (serial log: /x/serial.log)\nthe end of the log:\na"
    );
}

#[skuld::test]
fn with_context_with_step_with_tail_and_clone_keep_the_serial_log_and_the_source() {
    let base = Error::new(ErrorKind::ConnectionLost, "lost")
        .with_source(Marker)
        .with_serial_log("/x/serial.log");
    let cases = [
        (
            "with_context",
            base.clone().with_context(ErrorKind::Io, "reading"),
            "reading: lost",
            "reading: lost (serial log: /x/serial.log)",
        ),
        (
            "with_step",
            base.clone().with_step("try again"),
            "lost; try again",
            "lost; try again (serial log: /x/serial.log)",
        ),
        (
            "with_tail",
            base.clone().with_tail("the log", "a\nb"),
            "lost\nthe end of the log:\na\nb",
            "lost (serial log: /x/serial.log)\nthe end of the log:\na\nb",
        ),
        (
            "after_cleanup",
            base.clone().after_cleanup("the file", Err("locked")),
            "lost; cleaning up failed as well, so the file remains (locked)",
            "lost; cleaning up failed as well, so the file remains (locked) (serial log: /x/serial.log)",
        ),
        (
            "together_with",
            base.clone().together_with("a second failure"),
            "lost; a second failure as well",
            "lost; a second failure as well (serial log: /x/serial.log)",
        ),
        ("clone", base.clone(), "lost", "lost (serial log: /x/serial.log)"),
    ];
    for (name, error, body, message) in cases {
        assert_eq!(error.message(), message, "{name}");
        assert_eq!(error.body(), body, "{name}");
        assert_eq!(
            error.serial_log(),
            Some(std::path::Path::new("/x/serial.log")),
            "{name}"
        );
        assert!(is_marked(&error), "{name}: the source stays");
    }
}

#[skuld::test]
fn body_has_no_serial_log_path() {
    let error = Error::new(ErrorKind::Io, "cannot").with_serial_log("/x/serial.log");
    assert_eq!(error.body(), "cannot");
    assert_eq!(Error::new(ErrorKind::Io, "cannot").body(), "cannot");
}

#[skuld::test]
fn with_serial_log_replaces_an_earlier_path() {
    let error = Error::new(ErrorKind::Io, "cannot")
        .with_serial_log("/x/first.log")
        .with_serial_log("/y/second.log");
    assert_eq!(error.message(), "cannot (serial log: /y/second.log)");
    assert_eq!(error.serial_log(), Some(std::path::Path::new("/y/second.log")));
}

#[skuld::test]
fn with_appended_puts_the_text_at_the_end_after_the_serial_log_and_keeps_kind_and_source() {
    let error = Error::new(ErrorKind::Unavailable, "launch failed")
        .with_source(Marker)
        .with_serial_log("/x/serial.log")
        .with_tail("the log", "a")
        .with_appended("\nQEMU's output:\nnothing");
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert_eq!(
        error.body(),
        "launch failed\nthe end of the log:\na\nQEMU's output:\nnothing"
    );
    assert_eq!(
        error.message(),
        "launch failed (serial log: /x/serial.log)\nthe end of the log:\na\nQEMU's output:\nnothing"
    );
    assert!(is_marked(&error));
}

#[skuld::test]
fn embedded_is_the_kind_and_the_body_without_the_serial_log_path() {
    let error = Error::new(ErrorKind::ConnectionLost, "lost")
        .with_serial_log("/x/serial.log")
        .with_tail("the log", "a");
    assert_eq!(error.embedded(), "connection-lost: lost\nthe end of the log:\na");
    assert_eq!(Error::new(ErrorKind::Io, "cannot").embedded(), "io: cannot");
}

#[skuld::test]
fn or_serial_log_of_takes_the_path_of_the_other_error_only_when_there_is_none() {
    let with = |path: &str| Error::new(ErrorKind::Io, "a").with_serial_log(path);
    let without = || Error::new(ErrorKind::Io, "b");

    let taken = without().or_serial_log_of(&with("/x/other.log"));
    assert_eq!(taken.serial_log(), Some(std::path::Path::new("/x/other.log")));
    assert_eq!(taken.message(), "b (serial log: /x/other.log)");

    let kept = with("/x/own.log").or_serial_log_of(&with("/x/other.log"));
    assert_eq!(kept.serial_log(), Some(std::path::Path::new("/x/own.log")));

    assert_eq!(without().or_serial_log_of(&without()).serial_log(), None);
}

/// An error with a message of its own and a source.
#[derive(Debug)]
struct Layer(&'static str, Option<Box<dyn std::error::Error + 'static>>);

impl std::fmt::Display for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Layer {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.1.as_deref()
    }
}

#[skuld::test]
fn the_text_of_an_error_is_followed_by_each_source_in_turn() {
    let dns = Layer("dns error: no such host", None);
    let connect = Layer("client error (Connect)", Some(Box::new(dns)));
    let top = Layer("error sending request for url (https://h/x)", Some(Box::new(connect)));
    assert_eq!(
        error::text_with_sources(&top),
        "error sending request for url (https://h/x): client error (Connect): dns error: no such host"
    );
}

#[skuld::test]
fn a_source_the_text_already_says_is_not_repeated() {
    let inner = Layer("connection refused", None);
    let top = Layer("cannot connect: connection refused", Some(Box::new(inner)));
    assert_eq!(error::text_with_sources(&top), "cannot connect: connection refused");
}

#[skuld::test]
fn an_error_without_a_source_is_its_text() {
    assert_eq!(error::text_with_sources(&Layer("plain", None)), "plain");
}
