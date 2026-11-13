use std::io::Write;
use std::path::Path;

use super::{Launches, LogReader, Serial, read};
use crate::test_support::fixtures::unicode_home;
use crate::{ErrorKind, Result};

/// One finished launch whose serial log held `log`.
fn note(launches: &mut Launches, log: &[u8]) -> Result<()> {
    launches.note(&read(log))
}

/// Appends `bytes` to `path`, as the guest's serial port does.
fn append(path: &Path, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

#[skuld::test]
fn the_markers_are_read_by_their_fixed_prefix() {
    let log = "firmware noise\r\n\
               DEVVM-PROGRESS=specialize\r\n\
               DEVVM-PROGRESS=first logon: DEVVM user session 1\r\n\
               DEVVM-LICENSE={\"LicenseStatus\":1,\"GracePeriodRemaining\":5}\r\n\
               DEVVM-BASE-DONE\r\n";

    let serial = read(log.as_bytes());

    assert_eq!(serial.progress, ["specialize", "first logon: DEVVM user session 1"]);
    assert_eq!(
        serial.license.as_deref(),
        Some("DEVVM-LICENSE={\"LicenseStatus\":1,\"GracePeriodRemaining\":5}")
    );
    assert_eq!(serial.failed, None);
    assert!(serial.done);
}

#[skuld::test]
fn only_the_prefix_of_a_failed_marker_is_read_and_its_localized_text_is_kept_as_text() {
    let log = "DEVVM-BASE-FAILED=Der Zugriff auf den Pfad wurde verweigert: \u{fc}ber\r\nDEVVM-PROGRESS=late\r\n";

    let serial = read(log.as_bytes());

    assert_eq!(
        serial.failed.as_deref(),
        Some("Der Zugriff auf den Pfad wurde verweigert: \u{fc}ber")
    );
    assert!(!serial.done);
}

#[skuld::test]
fn a_marker_that_does_not_start_its_line_is_not_one() {
    let log = "echo DEVVM-BASE-DONE\nx DEVVM-PROGRESS=nope\nDEVVM-BASE-DONE and more\nDEVVM-BASE-DONEX\n";

    let serial = read(log.as_bytes());

    assert!(!serial.done, "{serial:?}");
    assert!(serial.progress.is_empty(), "{serial:?}");
}

#[skuld::test]
fn bytes_that_are_not_utf8_do_not_stop_the_read() {
    let mut log = b"\xff\xfe junk\nDEVVM-PROGRESS=after the junk\n".to_vec();
    log.extend_from_slice(b"DEVVM-BASE-FAILED=\xe4\xf6\n");

    let serial = read(&log);

    assert_eq!(serial.progress, ["after the junk"]);
    assert!(serial.failed.is_some());
}

#[skuld::test]
fn the_last_license_line_is_the_license() {
    let serial = read(b"DEVVM-LICENSE=null\nDEVVM-LICENSE={\"LicenseStatus\":1}\n");
    assert_eq!(serial.license.as_deref(), Some("DEVVM-LICENSE={\"LicenseStatus\":1}"));
}

#[skuld::test]
fn the_first_launch_needs_no_marker() {
    let mut launches = Launches::default();
    note(&mut launches, b"").unwrap();
}

#[skuld::test]
fn a_later_launch_needs_a_marker_no_earlier_launch_wrote() {
    let mut launches = Launches::default();
    note(&mut launches, b"").unwrap();
    note(&mut launches, b"DEVVM-PROGRESS=specialize\n").unwrap();
    note(
        &mut launches,
        b"DEVVM-PROGRESS=specialize\nDEVVM-PROGRESS=first logon\n",
    )
    .unwrap();

    let error = note(
        &mut launches,
        b"DEVVM-PROGRESS=specialize\nDEVVM-PROGRESS=first logon\n",
    )
    .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    let message = error.message();
    assert!(message.contains("launch 4"), "{message}");
    assert!(message.contains("first logon"), "{message}");
}

#[skuld::test]
fn a_second_launch_that_wrote_nothing_is_a_boot_loop_naming_that_no_marker_was_written() {
    let mut launches = Launches::default();
    note(&mut launches, b"").unwrap();

    let error = note(&mut launches, b"").unwrap_err();

    assert!(error.message().contains("launch 2"), "{error}");
    assert!(error.message().contains("no marker"), "{error}");
}

#[skuld::test]
fn the_last_marker_is_the_newest_of_every_launch() {
    let mut launches = Launches::default();
    note(&mut launches, b"DEVVM-PROGRESS=a\n").unwrap();
    note(&mut launches, b"DEVVM-PROGRESS=a\nDEVVM-PROGRESS=b\n").unwrap();
    assert_eq!(launches.last_marker(), Some("b"));
}

#[skuld::test]
fn a_poll_returns_only_the_markers_written_since_the_one_before(#[fixture(unicode_home)] home: &Path) {
    let log = home.join("serial.log");
    let mut reader = LogReader::new(&log);
    append(
        &log,
        b"DEVVM-PROGRESS=a
DEVVM-LICENSE=first
",
    );
    let first = reader.poll(false).unwrap();
    append(
        &log,
        b"DEVVM-PROGRESS=b
",
    );

    let second = reader.poll(false).unwrap();

    assert_eq!(first.progress, ["a"]);
    assert_eq!(first.license.as_deref(), Some("DEVVM-LICENSE=first"));
    assert_eq!(second.progress, ["b"]);
    assert_eq!(second.license, None, "the license is the first poll's");
    assert_eq!(reader.poll(false).unwrap(), Serial::default(), "nothing was written");
}

#[skuld::test]
fn a_line_the_guest_is_still_writing_waits_for_its_line_end(#[fixture(unicode_home)] home: &Path) {
    let log = home.join("serial.log");
    let mut reader = LogReader::new(&log);
    append(
        &log,
        b"DEVVM-PROGRESS=a
DEVVM-PROGRESS=spec",
    );

    let early = reader.poll(false).unwrap();
    append(
        &log, b"ialize
",
    );
    let late = reader.poll(false).unwrap();

    assert_eq!(early.progress, ["a"]);
    assert_eq!(late.progress, ["specialize"]);
}

#[skuld::test]
fn the_last_poll_takes_a_final_line_that_has_no_line_end(#[fixture(unicode_home)] home: &Path) {
    let log = home.join("serial.log");
    let mut reader = LogReader::new(&log);
    append(
        &log,
        b"DEVVM-PROGRESS=a
DEVVM-BASE-DONE",
    );

    let before = reader.poll(false).unwrap();
    let last = reader.poll(true).unwrap();

    assert!(!before.done);
    assert!(last.done);
    assert!(reader.whole().done);
    assert_eq!(reader.whole().progress, ["a"]);
}

#[skuld::test]
fn a_log_qemu_has_not_made_yet_holds_nothing_and_is_read_from_its_start_once_it_exists(
    #[fixture(unicode_home)] home: &Path,
) {
    let log = home.join("serial.log");
    let mut reader = LogReader::new(&log);

    assert_eq!(reader.poll(false).unwrap(), Serial::default());
    append(
        &log,
        b"DEVVM-PROGRESS=a
",
    );

    assert_eq!(reader.poll(false).unwrap().progress, ["a"]);
}

#[skuld::test]
fn the_whole_log_is_every_poll_together_with_the_last_license_and_failure(#[fixture(unicode_home)] home: &Path) {
    let log = home.join("serial.log");
    let mut reader = LogReader::new(&log);
    append(
        &log,
        b"DEVVM-PROGRESS=a
DEVVM-LICENSE=1
",
    );
    reader.poll(false).unwrap();
    append(
        &log,
        b"DEVVM-PROGRESS=b
DEVVM-LICENSE=2
DEVVM-BASE-FAILED=boom
",
    );
    reader.poll(true).unwrap();

    let whole = reader.whole();

    assert_eq!(whole.progress, ["a", "b"]);
    assert_eq!(whole.license.as_deref(), Some("DEVVM-LICENSE=2"));
    assert_eq!(whole.failed.as_deref(), Some("boom"));
    assert!(!whole.done);
}

#[skuld::test]
fn a_line_that_takes_several_polls_to_finish_is_read_once_and_each_poll_reads_only_past_the_bytes_it_has(
    #[fixture(unicode_home)] home: &Path,
) {
    let log = home.join("serial.log");
    let mut reader = LogReader::new(&log);
    let length = |path: &Path| std::fs::metadata(path).unwrap().len();

    append(&log, b"DEVVM-PROGRESS=spec");
    let first = reader.poll(false).unwrap();
    assert_eq!(
        reader.read,
        length(&log),
        "the unfinished line is taken in, not left to be read again"
    );
    append(&log, b"ial");
    let second = reader.poll(false).unwrap();
    assert_eq!(reader.read, length(&log));
    append(&log, b"ize\r\n");
    let third = reader.poll(false).unwrap();

    assert_eq!(first, Serial::default());
    assert_eq!(second, Serial::default());
    assert_eq!(third.progress, ["specialize"]);
    assert_eq!(reader.read, length(&log));
    assert_eq!(reader.poll(false).unwrap(), Serial::default());
}

#[skuld::test]
fn a_log_that_cannot_be_read_is_an_error_that_does_not_name_its_path(#[fixture(unicode_home)] home: &Path) {
    // A directory where the log should be: opening or reading it fails on every host.
    let log = home.join("a-directory-not-a-log");
    std::fs::create_dir(&log).unwrap();
    let mut reader = LogReader::new(&log);

    let error = reader.poll(false).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(!error.message().contains("a-directory-not-a-log"), "{error}");
    assert!(error.message().contains("serial log"), "{error}");
}
