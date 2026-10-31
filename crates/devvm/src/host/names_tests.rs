use std::path::Path;

use crate::host::names::{posix_problem, windows_problem};
use crate::test_support::fixtures::unicode_home;

#[skuld::test]
fn posix_names_are_anything_but_nothing_dots_slash_and_nul() {
    for name in [
        "a",
        "a b",
        "a\\b",
        "C:\\z",
        "con",
        "x.",
        "\u{416}",
        "line\nbreak",
        "...",
        "-dash",
    ] {
        assert_eq!(posix_problem(name), None, "{name:?}");
    }
    for name in ["", ".", "..", "a/b", "a\0b"] {
        assert!(posix_problem(name).is_some(), "{name:?}");
    }
}

#[skuld::test]
fn windows_refuses_what_is_not_one_file_name() {
    for name in [
        "a\\b", "..\\x", "C:\\z", "C:z", "a:b", "a<b", "a>b", "a\"b", "a|b", "a?b", "a*b", "co\u{7}", "a\u{1}",
        "a\u{1f}b", "", ".", "..", "a/b",
    ] {
        assert!(windows_problem(name).is_some(), "{name:?} was accepted");
    }
    for name in [
        "a",
        "a b",
        "\u{416}",
        "it's",
        "$x",
        "%x",
        "[a]",
        "-dash",
        "a.b",
        ".hidden",
        "\u{1f600}",
        // NTFS holds DEL and the C1 controls; only U+0001 to U+001F are illegal.
        "a\u{7f}",
        "\u{85}x",
        "\u{9f}",
    ] {
        assert_eq!(windows_problem(name), None, "{name:?}");
    }
}

/// A Unix file system holds 255 bytes per name, however many characters they are.
#[skuld::test]
fn posix_refuses_a_name_over_255_bytes() {
    let ok = [
        "a".repeat(255),
        "\u{e9}".repeat(127),
        "\u{4e2d}".repeat(85),
        "\u{1f600}".repeat(63),
    ];
    for name in ok {
        assert_eq!(posix_problem(&name), None, "{} bytes", name.len());
    }
    let too_long = [
        "a".repeat(256),
        "\u{e9}".repeat(128),
        "\u{4e2d}".repeat(86),
        "\u{1f600}".repeat(64),
    ];
    for name in too_long {
        let problem = posix_problem(&name).unwrap_or_else(|| panic!("{} bytes were accepted", name.len()));
        assert!(problem.contains("255 bytes"), "{problem}");
    }
}

/// NTFS holds 255 UTF-16 units per name: a character outside the BMP is two, and the bytes of UTF-8 do not count.
#[skuld::test]
fn windows_refuses_a_name_over_255_utf16_units() {
    let ok = ["a".repeat(255), "\u{4e2d}".repeat(255), "\u{1f600}".repeat(127)];
    for name in ok {
        assert_eq!(windows_problem(&name), None, "{} bytes", name.len());
    }
    let too_long = ["a".repeat(256), "\u{4e2d}".repeat(256), "\u{1f600}".repeat(128)];
    for name in too_long {
        let problem = windows_problem(&name).unwrap_or_else(|| panic!("{} bytes were accepted", name.len()));
        assert!(problem.contains("255 UTF-16 units"), "{problem}");
    }
}

// Measured on NTFS ====================================================================================================

/// Set by the CI job that runs on a Windows runner: the test below creates files with these names there.
const NTFS_CLAIM: &str = "DEVVM_COVERS_NTFS_NAMES";

/// `Ok` when this run claims an NTFS temp directory.
fn ntfs_claimed() -> Result<(), String> {
    match std::env::var(NTFS_CLAIM) {
        Ok(claimed) if !claimed.is_empty() => Ok(()),
        _ => Err(format!(
            "{NTFS_CLAIM} is not set; what NTFS stores is measured in CI (Test job on windows-latest)"
        )),
    }
}

/// Whether what is written to the file `path` is what is read from it, so the name is a file and not a device.
fn round_trip(path: &Path) -> Result<(), String> {
    std::fs::write(path, b"stored").map_err(|e| format!("write failed: {e}"))?;
    match std::fs::read(path) {
        Ok(read) if read == b"stored" => Ok(()),
        Ok(read) => Err(format!("read back {read:?}")),
        Err(e) => Err(format!("read failed: {e}")),
    }
}

/// What the file system does with the name `name`: `Ok` when a file made under it is listed under exactly that name and
/// keeps what is written to it, `Err` with what happened otherwise (the create failed, or Windows stored another name
/// or none, or the name is a device).
fn stored(dir: &Path, name: &str) -> Result<(), String> {
    let dir = dir.join("probe");
    std::fs::create_dir(&dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    let outcome = match std::fs::File::create(dir.join(name)) {
        Err(e) => Err(format!("create failed: {e}")),
        Ok(file) => {
            drop(file);
            let listed: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            match listed.iter().any(|listed| listed == name) {
                true => round_trip(&dir.join(name)),
                false => Err(format!("listed as {listed:?}")),
            }
        }
    };
    std::fs::remove_dir_all(&dir).unwrap();
    outcome
}

/// `windows_problem` is what NTFS does, for the names the rules name and the ones they did not know. Trailing dots and
/// spaces and device names are the copy contract's, so they are not probed.
#[skuld::test(requires = [ntfs_claimed])]
fn windows_problem_agrees_with_what_ntfs_stores(#[fixture(unicode_home)] home: &Path) {
    let names = [
        // Plain.
        "a",
        "a b",
        "\u{416}",
        "it's",
        "$x",
        "%x",
        "[a]",
        "-dash",
        "a.b",
        ".hidden",
        "\u{1f600}",
        // Characters Windows refuses.
        "C:z",
        "a:b",
        "a<b",
        "a>b",
        "a\"b",
        "a|b",
        "a?b",
        "a*b",
        "co\u{7}",
        "a\u{1}",
        "a\u{1f}b",
        // DEL and the C1 controls.
        "a\u{7f}",
        "\u{85}x",
        "\u{9f}",
    ];
    let mut table = String::new();
    let mut disagree = Vec::new();
    for name in names {
        let measured = stored(home, name);
        let rule = windows_problem(name);
        table.push_str(&format!(
            "{name:?}: NTFS {measured:?}; rule {rule:?}
"
        ));
        if measured.is_ok() != rule.is_none() {
            disagree.push(name);
        }
    }
    eprintln!("{table}");
    assert!(
        disagree.is_empty(),
        "windows_problem disagrees with NTFS on {disagree:?}:\n{table}"
    );
}

/// The trees `devvm cp` copies hold no trailing dots or spaces and no device names (a contract, not a check).
#[skuld::test]
fn windows_names_do_not_check_trailing_dots_spaces_or_device_names() {
    for name in ["x.", "x ", "con", "Aux.txt", "COM1", "CONIN$"] {
        assert_eq!(windows_problem(name), None, "{name:?}");
    }
}
