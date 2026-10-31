use std::path::Path;

use crate::host::names::{posix_problem, windows_key, windows_problem};
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
        "a\\b",
        "..\\x",
        "C:\\z",
        "C:z",
        "a:b",
        "a<b",
        "a>b",
        "a\"b",
        "a|b",
        "a?b",
        "a*b",
        "x.",
        "x ",
        "con",
        "CON",
        "Aux.txt",
        "nul.tar.gz",
        "com1",
        "LPT9.x",
        "com0",
        "COM0.txt",
        "lpt0",
        "LPT0",
        "COM\u{b9}",
        "com\u{b2}.txt",
        "COM\u{b3}",
        "LPT\u{b9}",
        "lpt\u{b2}",
        "LPT\u{b3}.x",
        "CONIN$",
        "conout$",
        "CONOUT$.txt",
        "CON .txt",
        "co\u{7}",
        "a\u{1}",
        "a\u{1f}b",
        "",
        ".",
        "..",
        "a/b",
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
        "com10",
        "COM4\u{b2}",
        "COM\u{b9}\u{b9}",
        "console",
        "conin",
        "CONIN$x",
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

#[skuld::test]
fn windows_keys_ignore_case_and_nothing_else() {
    assert_eq!(windows_key("a"), windows_key("A"));
    assert_eq!(windows_key("\u{e9}x"), windows_key("\u{c9}X"));
    assert_eq!(windows_key("\u{416}"), windows_key("\u{436}"));
    assert_ne!(windows_key("a"), windows_key("b"));
    // `\u{df}` upper-cases to two letters; it stays itself rather than becoming `SS`.
    assert_ne!(windows_key("\u{df}"), windows_key("SS"));
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

/// `Ok` when this run claims an NTFS temp directory. Creating the names opens Windows devices, so no other run does it.
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

/// `windows_problem` is what NTFS does, for the names the rules name and the ones they did not know.
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
        "console",
        "com10",
        // Characters and endings Windows refuses or drops.
        "C:z",
        "a:b",
        "a<b",
        "a>b",
        "a\"b",
        "a|b",
        "a?b",
        "a*b",
        "x.",
        "x ",
        "co\u{7}",
        "a\u{1}",
        "a\u{1f}b",
        // DEL and the C1 controls.
        "a\u{7f}",
        "\u{85}x",
        "\u{9f}",
        // Device names, with and without an extension and in lower case.
        "con",
        "CON",
        "Aux.txt",
        "nul.tar.gz",
        "PRN",
        "com1",
        "COM9",
        "LPT9.x",
        "com0",
        "COM0",
        "LPT0",
        "lpt0.txt",
        // Superscript digits, which Windows 11 reads as COM and LPT numbers.
        "COM\u{b9}",
        "COM\u{b2}",
        "COM\u{b3}",
        "LPT\u{b9}",
        "LPT\u{b2}",
        "LPT\u{b3}",
        "com\u{b9}.txt",
        "COM4\u{b2}",
        // Console handles.
        "CONIN$",
        "CONOUT$",
        "conin$",
        "CONOUT$.txt",
    ];
    // Measured on the windows-latest runner: NTFS stores the device names `CON`, `AUX.txt`, `COM1`, `COM0`, `LPT¹` and
    // `CONIN$` and reads them back, through Rust's std. `windows_problem` refuses them anyway: the owner's copy rules
    // name "reserved device names" among the names a target cannot store. The measurement is pinned here so a change
    // in Windows shows up, and the rule is not loosened by it.
    let refused_though_stored = [
        "con",
        "CON",
        "Aux.txt",
        "nul.tar.gz",
        "PRN",
        "com1",
        "COM9",
        "LPT9.x",
        "com0",
        "COM0",
        "LPT0",
        "lpt0.txt",
        "COM\u{b9}",
        "COM\u{b2}",
        "COM\u{b3}",
        "LPT\u{b9}",
        "LPT\u{b2}",
        "LPT\u{b3}",
        "com\u{b9}.txt",
        "CONIN$",
        "CONOUT$",
        "conin$",
        "CONOUT$.txt",
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
        let agrees = match refused_though_stored.contains(&name) {
            true => measured.is_ok() && rule.is_some(),
            false => measured.is_ok() == rule.is_none(),
        };
        if !agrees {
            disagree.push(name);
        }
    }
    eprintln!("{table}");
    assert!(
        disagree.is_empty(),
        "windows_problem disagrees with NTFS on {disagree:?}:\n{table}"
    );
}
