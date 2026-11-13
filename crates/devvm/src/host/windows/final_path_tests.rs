use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;

use super::{of_handle, plain};
use crate::host::windows::MAX_PLAIN_PATH_UNITS;
use crate::test_support::fixtures::unicode_home;

fn plain_of(text: &str) -> OsString {
    let units: Vec<u16> = text.encode_utf16().collect();
    OsString::from_wide(&plain(units))
}

#[skuld::test]
fn a_verbatim_disk_path_is_written_plain() {
    assert_eq!(plain_of(r"\\?\C:\iso\win.iso"), r"C:\iso\win.iso");
}

#[skuld::test]
fn a_verbatim_share_path_is_written_as_a_share() {
    assert_eq!(plain_of(r"\\?\UNC\server\share\win.iso"), r"\\server\share\win.iso");
}

#[skuld::test]
fn a_path_that_is_not_a_disk_or_share_path_stays_as_it_is() {
    assert_eq!(
        plain_of(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\win.iso"),
        r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\win.iso"
    );
}

#[skuld::test]
fn a_path_too_long_to_be_plain_stays_verbatim() {
    let long = format!(r"\\?\C:\{}", "a".repeat(MAX_PLAIN_PATH_UNITS));
    assert_eq!(plain_of(&long), long.as_str());
    let short = format!(r"\\?\C:\{}", "a".repeat(MAX_PLAIN_PATH_UNITS - 3));
    assert_eq!(plain_of(&short), &short[4..]);
}

#[skuld::test]
fn the_final_path_of_an_open_file_is_the_plain_long_name_of_that_file(#[fixture(unicode_home)] home: &Path) {
    let file = home.join("window.iso");
    std::fs::write(&file, b"x").unwrap();
    let open = std::fs::File::open(&file).unwrap();

    let found = of_handle(&open).unwrap();

    assert!(
        found.is_absolute() && !found.to_string_lossy().starts_with(r"\\?\"),
        "{}",
        found.display()
    );
    assert!(found.ends_with("window.iso"), "{}", found.display());
    assert_eq!(std::fs::read(&found).unwrap(), b"x");
}
