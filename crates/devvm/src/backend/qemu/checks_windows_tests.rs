//! The qemu line where the system emulator is a file that its DACL does not let anyone read.

use std::path::Path;

use super::checks_tests::brief_and_full_agree_on_an_emulator_that_cannot_be_read;
use crate::test_support::file_security::set_protected_dacl;
use crate::test_support::fixtures::unicode_home;

/// An `OWNER RIGHTS` entry of `DELETE` and `SYNCHRONIZE` replaces the owner's implicit rights, so the file cannot be
/// opened to read, and the test can still remove it.
#[skuld::test]
fn an_emulator_that_cannot_be_opened_leaves_the_brief_and_full_lines_agreeing(#[fixture(unicode_home)] home: &Path) {
    brief_and_full_agree_on_an_emulator_that_cannot_be_read(home, |system| {
        set_protected_dacl(system, "D:P(A;;0x110000;;;OW)");
    });
}
