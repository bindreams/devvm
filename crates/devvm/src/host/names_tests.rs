use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::host::names::{posix_problem, unix_run_dir, windows_colon_path, windows_problem, windows_run_dir};
use crate::random;
use crate::test_groups::{Group, ntfs_names};
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
#[skuld::test]
fn windows_problem_agrees_with_what_ntfs_stores(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(ntfs_names)] _group: &Group,
) {
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
        "windows_problem disagrees with NTFS on {disagree:?} \
         (the group needs an NTFS temporary directory; {}):\n{table}",
        ntfs_names::OPT_OUT
    );
}

/// The trees `devvm cp` copies hold no trailing dots or spaces and no device names (a contract, not a check).
#[skuld::test]
fn windows_names_do_not_check_trailing_dots_spaces_or_device_names() {
    for name in ["x.", "x ", "con", "Aux.txt", "COM1", "CONIN$"] {
        assert_eq!(windows_problem(name), None, "{name:?}");
    }
}

#[skuld::test]
fn windows_reads_a_colon_in_any_segment_as_a_stream_or_a_drive() {
    for path in ["a:b.txt", "d:x/f.txt", "d::$DATA/f.txt", "dir/f:g", "dir/a:b/c", "C:"] {
        assert!(windows_colon_path(path), "{path:?}");
    }
    for path in ["a", "a/b", "dir/\u{416}.txt", "a b/c d", "a|b/c", "back\\slash"] {
        assert!(!windows_colon_path(path), "{path:?}");
    }
}

#[skuld::test]
fn the_unix_default_is_tmpdir_then_slash_tmp_with_the_euid() {
    let tmp = std::env::temp_dir();
    assert_eq!(
        unix_run_dir(Some(tmp.clone().into_os_string()), 501),
        tmp.join("devvm-run-501")
    );
    assert_eq!(unix_run_dir(None, 0), PathBuf::from("/tmp/devvm-run-0"));
    assert_eq!(
        unix_run_dir(Some(OsString::new()), 7),
        PathBuf::from("/tmp/devvm-run-7")
    );
    assert_eq!(
        unix_run_dir(Some(OsString::from("rel")), 7),
        PathBuf::from("/tmp/devvm-run-7")
    );
}

/// The 52-character SID of a user and the 63-character `S-1-5-80-...` SID of a service.
const USER_SID: &str = "S-1-5-21-4294967295-4294967295-4294967295-4294967295";
const SERVICE_SID: &str = "S-1-5-80-4294967295-4294967295-4294967295-4294967295-4294967295";

#[skuld::test]
fn the_directory_name_is_16_hex_of_the_sid_hash_for_any_sid_length() {
    assert_eq!(USER_SID.len(), 52);
    assert_eq!(SERVICE_SID.len(), 63);
    let program_data = Path::new(r"C:\ProgramData");
    for sid in [USER_SID, SERVICE_SID, "S-1-5-18"] {
        let dir = windows_run_dir(program_data, sid);
        let name = dir.file_name().unwrap().to_str().unwrap();
        let hex = name.strip_prefix("devvm-run-").unwrap_or_else(|| panic!("{name}"));
        assert!(random::is_hex(hex, 8), "{name}");
        assert_eq!(dir.parent().unwrap(), program_data);
        // The socket path under it, `<dir>\<16 hex>.sock`, is the same length for every account.
        assert_eq!(dir.to_string_lossy().len() + 1 + 21, 63, "{sid}");
    }
}

#[skuld::test]
fn two_sids_give_two_names_and_one_sid_gives_one() {
    let program_data = Path::new(r"C:\ProgramData");
    assert_eq!(
        windows_run_dir(program_data, USER_SID),
        windows_run_dir(program_data, USER_SID)
    );
    assert_ne!(
        windows_run_dir(program_data, USER_SID),
        windows_run_dir(program_data, SERVICE_SID)
    );
}

/// The name is the first 8 bytes of SHA-256 over the SID string. The literal is a stability pin: a name made by an
/// earlier build must not change.
#[skuld::test]
fn the_run_dir_name_of_a_sid_does_not_change() {
    let dir = windows_run_dir(Path::new(r"C:\ProgramData"), "S-1-5-18");
    assert_eq!(dir.file_name().unwrap(), "devvm-run-593347bdfcc9bfa7");
}
