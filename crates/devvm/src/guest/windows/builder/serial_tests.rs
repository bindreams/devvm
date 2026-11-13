use std::path::Path;

use super::{Launches, read};
use crate::{ErrorKind, Result};

/// One finished launch whose serial log held `log`.
fn note(launches: &mut Launches, log: &[u8]) -> Result<()> {
    launches.note(&read(log), Path::new("serial.log"))
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
    assert!(message.contains("serial.log"), "{message}");
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
