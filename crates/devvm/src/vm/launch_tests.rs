use std::error::Error as _;

use super::survivor;
use crate::{Error, ErrorKind};

#[skuld::test]
fn a_machine_that_could_not_be_killed_is_named_in_the_start_failure_which_keeps_its_source_and_serial_log() {
    let cause = Error::new(ErrorKind::Unavailable, "the guest did not answer")
        .with_source(std::fmt::Error)
        .with_serial_log("/x/serial.log");
    let kill = Error::new(ErrorKind::Io, "cannot kill the VM process 7");

    let error = survivor(cause, kill, "its record is kept");

    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert_eq!(
        error.message(),
        "the guest did not answer; the machine that failed to start could not be killed either (io: cannot kill the \
         VM process 7) and may still run: its record is kept (serial log: /x/serial.log)"
    );
    assert!(
        error.source().is_some_and(|source| source.is::<std::fmt::Error>()),
        "the cause's source: {error}"
    );
}
