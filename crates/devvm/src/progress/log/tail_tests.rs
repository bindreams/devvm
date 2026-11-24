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
    // The cap keeps the end of the line: 8 KiB, the last 8 bytes of which are the text after the x's.
    let kept = format!("...{}{}", "x".repeat(TAIL_LINE_BYTES - " the end".len()), " the end");
    assert_eq!(
        error.message(),
        format!("the guest finished early\nthe end of the serial log:\nfirst\n{kept}")
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

/// `bytes` read in chunks of `chunk` bytes, as a file is.
fn tail_in_chunks(bytes: &[u8], chunk: usize) -> String {
    tail_of(std::io::BufReader::with_capacity(chunk, std::io::Cursor::new(bytes))).unwrap()
}

#[skuld::test]
fn a_line_of_exactly_the_cap_is_kept_whole_whatever_ends_it() {
    let line = "x".repeat(TAIL_LINE_BYTES);
    for end in ["", "\n", "\r\n"] {
        let text = tail_of(std::io::Cursor::new(format!("{line}{end}"))).unwrap();
        assert!(text == line, "ended by {end:?}: {text:.20} ({} bytes)", text.len());
    }
}

#[skuld::test]
fn a_line_one_byte_over_the_cap_is_cut_and_marked_whatever_ends_it() {
    let kept = format!("...{}", "x".repeat(TAIL_LINE_BYTES));
    let line = "x".repeat(TAIL_LINE_BYTES + 1);
    for end in ["", "\n", "\r\n"] {
        let text = tail_of(std::io::Cursor::new(format!("{line}{end}"))).unwrap();
        assert!(text == kept, "ended by {end:?}: {text:.20} ({} bytes)", text.len());
    }
}

#[skuld::test]
fn a_crlf_split_between_two_chunks_ends_the_line_without_the_cr() {
    assert_eq!(tail_in_chunks(b"ab\r\ncd\r\n", 3), "ab\ncd");
    // A line of exactly the cap, with its `\r` the last byte of one chunk and its `\n` the first of the next.
    let line = "x".repeat(TAIL_LINE_BYTES);
    let text = tail_in_chunks(format!("{line}\r\nnext\r\n").as_bytes(), TAIL_LINE_BYTES + 1);
    assert!(text == format!("{line}\nnext"), "{text:.20} ({} bytes)", text.len());
}

#[skuld::test]
fn a_line_after_a_cut_one_is_not_marked() {
    let long = "x".repeat(3 * TAIL_LINE_BYTES);
    let text = tail_in_chunks(format!("{long}\nshort\nlast").as_bytes(), 1000);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].starts_with("...x"), "{:.20}", lines[0]);
    assert_eq!(&lines[1..], ["short", "last"]);
}

#[skuld::test]
fn bytes_that_are_not_utf8_are_replaced() {
    assert_eq!(
        tail_of(std::io::Cursor::new(b"ok \xff\xfe end\r\n".to_vec())).unwrap(),
        "ok \u{fffd}\u{fffd} end"
    );
}
