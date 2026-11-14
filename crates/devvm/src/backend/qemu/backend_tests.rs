use std::path::Path;

use super::log_length;
use crate::test_groups::{Group, symlinks};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::symlink::symlink;

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
