use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;

use super::WindowsHost;
use crate::host::{Entry, Host};
use crate::test_support::fixtures::unicode_home;

#[skuld::test]
fn a_junction_reads_as_the_path_it_was_made_for_not_its_nt_name(#[fixture(unicode_home)] home: &Path) {
    let target = home.join("target");
    std::fs::create_dir(&target).unwrap();
    let link = home.join("junction");
    junction::create(&target, &link).unwrap();
    let text = WindowsHost.read_link(&link).unwrap();
    assert!(!text.contains("??"), "{text}");
    assert_eq!(text, target.display().to_string().replace('\\', "/"));
}

fn strip(name: &str, prefix: &str) -> Option<String> {
    WindowsHost.env_name_strip_prefix(name.as_ref(), prefix)
}

#[skuld::test]
fn ascii_case_does_not_matter_to_an_environment_name() {
    assert_eq!(strip("git_dir", "GIT_").as_deref(), Some("dir"));
    assert_eq!(strip("Git_Config_Count", "GIT_CONFIG_COUNT").as_deref(), Some(""));
    assert_eq!(strip("GIT", "GIT_"), None);
    assert_eq!(strip("GITX_DIR", "GIT_"), None);
}

/// Rust uppercases U+0131 to `I` and U+017F to `S`; Windows folds neither to ASCII.
#[skuld::test]
fn a_letter_rust_folds_to_ascii_and_windows_does_not_is_not_that_letter() {
    assert_eq!('\u{131}'.to_uppercase().to_string(), "I");
    assert_eq!('\u{17F}'.to_uppercase().to_string(), "S");
    assert_eq!(strip("G\u{131}T_DIR", "GIT_"), None);
    assert_eq!(strip("GIT_CONFIG_PARAMETER\u{17F}", "GIT_CONFIG_PARAMETERS"), None);
}

#[skuld::test]
fn the_rest_of_a_name_is_returned_as_the_name_has_it() {
    assert_eq!(
        strip("GIT_\u{131}\u{1F600}x", "GIT_").as_deref(),
        Some("\u{131}\u{1F600}x")
    );
    let lone_surrogate = OsString::from_wide(&[b'g'.into(), b'i'.into(), b't'.into(), b'_'.into(), 0xD800]);
    assert_eq!(
        WindowsHost.env_name_strip_prefix(&lone_surrogate, "GIT_").as_deref(),
        Some("\u{FFFD}")
    );
}

fn open(root: &Path, rel: &str) -> Entry {
    WindowsHost
        .open_entry(root, rel)
        .unwrap_or_else(|e| panic!("open_entry({rel}): {e}"))
}

#[skuld::test]
fn a_junction_at_the_end_is_a_symlink(#[fixture(unicode_home)] home: &Path) {
    let target = home.join("target");
    std::fs::create_dir(&target).unwrap();
    junction::create(&target, home.join("junction")).unwrap();
    assert!(matches!(open(home, "junction"), Entry::Symlink));
}

#[skuld::test]
fn a_name_windows_cannot_hold_is_absent(#[fixture(unicode_home)] home: &Path) {
    assert!(matches!(open(home, "a|b"), Entry::Absent));
}

/// `d\a:b` reads the stream `b` of the file `d\a`, which is no file git tracks.
#[skuld::test]
fn a_name_with_a_colon_is_absent_and_a_stream_is_never_read(#[fixture(unicode_home)] home: &Path) {
    std::fs::create_dir(home.join("d")).unwrap();
    std::fs::write(home.join("d").join("a"), b"the file").unwrap();
    // NTFS reads `:` as a stream separator whatever the prefix; the `\\?\` prefix is dropped to leave an ordinary path.
    let text = home.to_str().expect("the test directory is Unicode");
    let stream = format!(r"{}\d\a:b", text.strip_prefix(r"\\?\").unwrap_or(text));
    std::fs::write(&stream, b"the stream").unwrap();
    assert_eq!(
        std::fs::read(&stream).unwrap(),
        b"the stream",
        "there is a stream to be read"
    );
    for rel in ["d/a:b", "d/a:b/below", "d/a::$DATA", "a:b"] {
        assert!(matches!(open(home, rel), Entry::Absent), "{rel}");
    }
}
