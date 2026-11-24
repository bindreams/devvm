use std::error::Error as _;
use std::path::Path;

use super::{TAIL_LINE_BYTES, tail_of};
use crate::error::{Error, ErrorKind};
use crate::test_support::fixtures::unicode_home;

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

#[skuld::test]
fn a_cut_line_of_a_log_starts_with_dots_and_whole_characters(#[fixture(unicode_home)] home: &Path) {
    let log = home.join("serial.log");
    // The line is longer than the cap, which is not a multiple of 3: the cut falls inside a 3-byte character.
    std::fs::write(&log, "\u{20ac}".repeat(4000)).unwrap();
    let error = failed().with_log_tail("the serial log", &log);
    let (_, line) = error.message().split_once("the end of the serial log:\n").unwrap();
    let kept = line.strip_prefix("...").unwrap_or_else(|| panic!("{line:.20}"));
    assert!(!kept.is_empty() && kept.chars().all(|c| c == '\u{20ac}'), "{line:.20}");
    assert!(kept.len() <= TAIL_LINE_BYTES);

    // A line that fits is not marked.
    std::fs::write(&log, "short\n").unwrap();
    let error = failed().with_log_tail("the serial log", &log);
    assert!(error.message().ends_with("\nshort"), "{}", error.message());
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
