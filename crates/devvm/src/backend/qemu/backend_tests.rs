use std::path::Path;

use super::{log_length, unkilled};
use crate::test_groups::{Group, symlinks};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::symlink::symlink;
use crate::{Error, ErrorKind};

#[skuld::test]
fn a_frozen_qemu_that_could_not_be_killed_is_named_in_the_launch_failure_which_keeps_its_source_and_serial_log() {
    let error = Error::new(ErrorKind::Io, "cannot give the launch's hold to QEMU")
        .with_source(std::fmt::Error)
        .with_serial_log("/x/serial.log");
    let kill = Error::new(ErrorKind::Io, "cannot kill the VM process 7").with_serial_log("/y/other.log");

    let error = unkilled(error, &kill);

    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(error.serial_log(), Some(Path::new("/x/serial.log")));
    assert_eq!(
        error.message(),
        "cannot give the launch's hold to QEMU; the frozen QEMU could not be killed either (io: cannot kill the VM \
         process 7) and is still running (serial log: /x/serial.log)"
    );
    let source = std::error::Error::source(&error).expect("the launch failure's source");
    assert!(source.is::<std::fmt::Error>(), "{source:?}");
}

#[skuld::test]
fn a_log_that_is_not_there_has_length_0(#[fixture(unicode_home)] home: &Path) {
    assert_eq!(log_length(&home.join("qemu.log")).unwrap(), 0);
}

#[skuld::test]
fn a_log_is_as_long_as_the_file_it_is_written_in(#[fixture(unicode_home)] home: &Path) {
    let log = home.join("qemu.log");
    std::fs::write(&log, "an earlier launch\n").unwrap();

    assert_eq!(log_length(&log).unwrap(), "an earlier launch\n".len() as u64);
}

#[skuld::test]
fn a_log_that_is_a_link_has_the_length_of_the_file_qemu_appends_to(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(symlinks)] _group: &Group,
) {
    let target = home.join("elsewhere.log");
    std::fs::write(&target, "an earlier launch, longer than a link's own size\n").unwrap();
    let log = home.join("qemu.log");
    symlink(&target, &log);

    assert_eq!(
        log_length(&log).unwrap(),
        std::fs::metadata(&target).unwrap().len(),
        "a link's own length is the length of its target's path"
    );
}

#[skuld::test]
fn a_log_below_a_file_is_not_there(#[fixture(unicode_home)] home: &Path) {
    let file = home.join("a-file");
    std::fs::write(&file, "x").unwrap();

    assert_eq!(log_length(&file.join("qemu.log")).unwrap(), 0);
}
