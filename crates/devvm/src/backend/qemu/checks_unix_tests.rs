//! The qemu line where the system emulator is a file with no read permission.

use std::path::Path;

use super::checks_tests::brief_and_full_agree_on_an_emulator_that_cannot_be_read;
use crate::test_support::file_mode::set_mode;
use crate::test_support::fixtures::unicode_home;

/// Modes do not bind root, so the test refuses to run as root.
#[skuld::test]
fn an_emulator_without_read_permission_leaves_the_brief_and_full_lines_agreeing(#[fixture(unicode_home)] home: &Path) {
    // SAFETY: `geteuid` reads a process attribute.
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "file modes do not bind root: run this test as another user"
    );

    brief_and_full_agree_on_an_emulator_that_cannot_be_read(home, |system| set_mode(system, 0o000));
}
