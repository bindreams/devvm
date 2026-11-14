use super::{SCRATCH_DIR, script};
use crate::RunAs;
use crate::guest::GuestOs;
use crate::guest::windows::WindowsGuest;
use crate::guest::windows::wrap::literal;

/// The paths a copy step can be given: a space, a quote, `$`, a backtick, a bracket, a PowerShell quote that is not
/// `'`, a non-ASCII letter and a surrogate pair.
const HOSTILE: &[&str] = &[
    "C:/Users/user/a b",
    "C:/Users/user/it's",
    "C:/Users/user/$env:TEMP",
    "C:/Users/user/`n[1]",
    "C:/Users/user/\u{2018}x\u{2019}",
    "C:/Users/user/\u{e9} \u{416} \u{4e16} \u{1f600}",
    "C:/Users/user/$(Remove-Item -Recurse C:/)",
];

fn script_of(destination: &str, archive: &str) -> String {
    script(destination, archive)
}

#[skuld::test]
fn the_windows_extract_command_runs_pwsh_and_names_neither_sh_nor_tar() {
    let command = WindowsGuest
        .extract_command("C:/Users/user/dest", "C:/ProgramData/devvm/devvm-copy-1.tar")
        .unwrap();

    assert_eq!(command.argv[0], "pwsh");
    assert_eq!(
        command.run_as,
        RunAs::User,
        "a copy extracts into the user's own directories"
    );
    assert!(command.cwd.is_none());
    let script = command.argv.last().unwrap();
    assert!(script.contains("System.Formats.Tar.TarReader"), "{script}");
    assert!(script.contains("ExtractToFile"), "{script}");
    assert!(!script.contains("ExtractToDirectory"), "{script}");
    for word in ["tar.exe", "tar -xf", "/bin/sh", "sh -c", "bash"] {
        assert!(!script.contains(word), "`{word}` in {script}");
    }
}

#[skuld::test]
fn the_extraction_overwrites_and_the_archive_is_removed_whatever_the_extraction_did() {
    let script = script_of("C:/d", "C:/a.tar");
    let at = |needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` is not in {script}"))
    };
    // Overwriting, as `tar -xf` does.
    assert!(script.contains("ExtractToFile($path, $true)"), "{script}");
    // The destination is made first, the archive removed in a `finally`, and the script ends with the status.
    assert!(at("CreateDirectory") < at("ExtractToFile"), "{script}");
    assert!(
        at("finally") < at("Remove-Item") && at("Remove-Item") < at("exit $status"),
        "{script}"
    );
    assert!(
        script.contains("-ErrorAction Stop"),
        "a file that cannot be removed is an error: {script}"
    );
}

#[skuld::test]
fn each_entry_clears_its_own_file_before_the_write_restores_it_when_the_write_fails_and_marks_it_after() {
    let script = script_of("C:/d", "C:/a.tar");
    let at = |needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` is not in {script}"))
    };
    // One reader loop: no list of files built beforehand, nothing cleared or marked outside the entry's own turn.
    assert!(!script.contains("$files"), "{script}");
    assert_eq!(script.matches("-bnot $cleared").count(), 1, "{script}");
    assert_eq!(script.matches("GetNextEntry").count(), 1, "{script}");
    // Read-only, hidden and system are what makes the write fail.
    assert!(
        script.contains("[System.IO.FileAttributes]::ReadOnly -bor [System.IO.FileAttributes]::Hidden -bor [System.IO.FileAttributes]::System"),
        "{script}"
    );
    // An entry with no owner-write bit is the one that gets the attribute, as `tar -xf` makes the file read-only.
    assert!(script.contains("UnixFileMode]::UserWrite"), "{script}");
    assert!(
        at("GetNextEntry") < at("-bnot $cleared")
            && at("-bnot $cleared") < at("ExtractToFile")
            && at("ExtractToFile") < at("SetAttributes($path, $saved)")
            && at("SetAttributes($path, $saved)") < at("-bor [System.IO.FileAttributes]::ReadOnly"),
        "the attributes come back only when the write failed, never after the file is marked: {script}"
    );
    assert!(
        at("cannot mark") > at("-bor [System.IO.FileAttributes]::ReadOnly"),
        "a failed marking names the entry: {script}"
    );
}

#[skuld::test]
fn an_entry_that_is_not_a_file_or_a_directory_or_lies_outside_ends_the_step_before_it_is_written() {
    let script = script_of("C:/d", "C:/a.tar");
    let at = |needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` is not in {script}"))
    };
    assert!(at("lies outside the destination") < at("ExtractToFile"), "{script}");
    assert!(
        at("which a Windows guest does not extract") < at("ExtractToFile"),
        "{script}"
    );
}

#[skuld::test]
fn a_hostile_destination_or_archive_path_stays_one_literal_in_the_windows_script() {
    let plain = script_of("C:/d", "C:/a.tar");
    for hostile in HOSTILE {
        let script = script_of(hostile, "C:/ProgramData/devvm/devvm-copy-1.tar");

        // Only the literal differs: the rest of the script is the one a harmless path gets.
        let with_literal = script.replace(&format!("$destination = {}", literal(hostile)), "$destination = <d>");
        let expected = plain.replace(&format!("$destination = {}", literal("C:/d")), "$destination = <d>");
        let archive = literal("C:/ProgramData/devvm/devvm-copy-1.tar");
        assert_eq!(
            with_literal.replace(&archive, "<a>"),
            expected.replace(&literal("C:/a.tar"), "<a>"),
            "{hostile}"
        );
        assert!(
            script.contains(&format!("$destination = {}", literal(hostile))),
            "{hostile}"
        );
    }
}

#[skuld::test]
fn the_scratch_directory_is_one_the_bootstrap_makes_and_the_users_sftp_can_write() {
    assert_eq!(WindowsGuest.copy_scratch_dir(), SCRATCH_DIR);
    // `C:\ProgramData\devvm` is made with `Users` modify by the first-logon script, and it is where the boot task and
    // the elevated helper keep their files too.
    assert_eq!(
        SCRATCH_DIR.replace('/', "\\"),
        crate::guest::windows::templates::DEVVM_DIR
    );
    // No environment variable: SFTP expands none.
    assert!(!SCRATCH_DIR.contains('%') && !SCRATCH_DIR.contains('$'));
}
