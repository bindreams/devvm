use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use super::{WindowsHost, user, wide};
use crate::host::names::windows_run_dir;
use crate::host::{Entry, HoldOutcome, Host};
use crate::test_support::file_security::{Security, set_protected_dacl};
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

#[skuld::test]
fn the_default_is_programdata_devvm_run_and_the_token_sid() {
    let sid = user::current_sid().unwrap();
    assert!(sid.starts_with("S-1-5-"), "{sid}");
    let program_data = user::program_data().unwrap();
    let from_env = std::env::var_os("ProgramData").expect("ProgramData is set");
    assert!(
        program_data
            .to_string_lossy()
            .eq_ignore_ascii_case(&from_env.to_string_lossy()),
        "{} against {}",
        program_data.display(),
        from_env.to_string_lossy()
    );
    // The environment does not move it.
    let dir = WindowsHost.run_dir_default(&|_| Some(r"Z:\elsewhere".into())).unwrap();
    assert_eq!(dir, windows_run_dir(&program_data, &sid));
}

// Paths given to the Win32 calls --------------------------------------------------------------------------------------

/// `path` with a NUL and a few more characters after it: a Win32 call reads it as far as the NUL.
pub(super) fn with_nul(path: &Path) -> PathBuf {
    let mut units: Vec<u16> = path.as_os_str().encode_wide().collect();
    units.extend([0, u16::from(b'x')]);
    PathBuf::from(OsString::from_wide(&units))
}

/// A directory under `home` whose path is longer than `MAX_PATH`.
pub(super) fn long_dir(home: &Path) -> PathBuf {
    let dir = home.join("a".repeat(100)).join("b".repeat(100)).join("c".repeat(100));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(dir.as_os_str().len() > 260, "{}", dir.display());
    dir
}

/// The converted text of `path`, without the terminating NUL.
fn converted(path: &str) -> String {
    let mut units = wide(Path::new(path)).unwrap();
    assert_eq!(units.pop(), Some(0));
    String::from_utf16(&units).unwrap()
}

/// A short path stays plain, so Win32 reads its names by its name rules, as `std::fs` does.
#[skuld::test]
fn a_short_path_is_made_absolute_and_stays_plain_and_a_verbatim_or_device_path_stays() {
    assert_eq!(converted(r"C:\a\b"), r"C:\a\b");
    assert_eq!(converted("C:/a/../b"), r"C:\b");
    assert_eq!(converted(r"\\server\share\x"), r"\\server\share\x");
    assert_eq!(converted(r"\\?\C:\a\b"), r"\\?\C:\a\b");
    assert_eq!(converted(r"\\?\UNC\server\share\x"), r"\\?\UNC\server\share\x");
    assert_eq!(converted(r"\\.\pipe\x"), r"\\.\pipe\x");
}

/// `std`'s `get_long_path` prefixes a path once its units and the NUL reach `LEGACY_MAX_PATH` (248): 247 units.
#[skuld::test]
fn a_path_of_247_units_gets_the_verbatim_prefix_and_one_of_246_does_not() {
    let disk = |units: usize| format!(r"C:\{}", "a".repeat(units - r"C:\".len()));
    assert_eq!(converted(&disk(246)), disk(246));
    assert_eq!(converted(&disk(247)), format!(r"\\?\{}", disk(247)));
    let share = |units: usize| format!(r"\\server\share\{}", "a".repeat(units - r"\\server\share\".len()));
    assert_eq!(converted(&share(246)), share(246));
    assert_eq!(converted(&share(247)), format!(r"\\?\UNC\{}", &share(247)[2..]));
}

#[skuld::test]
fn publishing_a_directory_with_a_nul_in_either_path_is_invalid_input_and_moves_nothing(
    #[fixture(unicode_home)] home: &Path,
) {
    let (from, to) = (home.join("from"), home.join("to"));
    std::fs::create_dir(&from).unwrap();

    let source = WindowsHost.publish_dir(&with_nul(&from), &to).unwrap_err();
    let target = WindowsHost.publish_dir(&from, &with_nul(&to)).unwrap_err();

    assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput, "{source}");
    assert_eq!(target.kind(), std::io::ErrorKind::InvalidInput, "{target}");
    assert!(from.exists());
    assert!(!to.exists());
}

#[skuld::test]
fn creating_the_runtime_dir_with_a_nul_in_the_path_is_invalid_input_and_makes_nothing(
    #[fixture(unicode_home)] home: &Path,
) {
    let dir = home.join("run");

    let error = WindowsHost.create_runtime_dir(&with_nul(&dir)).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error}");
    assert!(!dir.exists());
}

#[skuld::test]
fn a_deleting_name_deeper_than_max_path_is_removed(#[fixture(unicode_home)] home: &Path) {
    let path = long_dir(home).join("x.sock.hold.del");
    std::fs::write(&path, b"left").unwrap();

    WindowsHost.remove_deleting_name(&path).unwrap();

    assert!(!path.exists());
}

#[skuld::test]
fn a_directory_deeper_than_max_path_is_published(#[fixture(unicode_home)] home: &Path) {
    let parent = long_dir(home);
    let (from, to) = (parent.join("from"), parent.join("to"));
    std::fs::create_dir(&from).unwrap();
    std::fs::write(from.join("f"), b"in").unwrap();

    WindowsHost.publish_dir(&from, &to).unwrap();

    assert!(!from.exists());
    assert_eq!(std::fs::read(to.join("f")).unwrap(), b"in");
}

#[skuld::test]
fn a_hold_deeper_than_max_path_is_deleted_and_leaves_no_name(#[fixture(unicode_home)] home: &Path) {
    let parent = long_dir(home);
    let path = parent.join("x.sock.hold");
    let hold = WindowsHost.hold_new(&path).unwrap();

    hold.delete().unwrap();

    // POSIX deletion: the name goes with the hold's file object, whoever else has the file open.
    assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 0);
    assert!(matches!(WindowsHost.hold_existing(&path).unwrap(), HoldOutcome::Absent));
}

// The runtime directory -----------------------------------------------------------------------------------------------

#[skuld::test]
fn a_junction_at_the_runtime_dir_path_is_refused(#[fixture(unicode_home)] home: &Path) {
    let target = home.join("target");
    std::fs::create_dir(&target).unwrap();
    let link = home.join("run");
    junction::create(&target, &link).unwrap();
    let error = WindowsHost.create_runtime_dir(&link).unwrap_err();
    assert!(error.to_string().contains("link"), "{error}");
}

#[skuld::test]
fn a_junction_at_the_runtime_dir_path_is_refused_by_the_check_that_makes_nothing(#[fixture(unicode_home)] home: &Path) {
    let target = home.join("target");
    std::fs::create_dir(&target).unwrap();
    let link = home.join("run");
    junction::create(&target, &link).unwrap();
    let error = WindowsHost.accept_runtime_dir(&link).unwrap_err();
    assert!(error.to_string().contains("link"), "{error}");
}

/// An `OWNER RIGHTS` entry (here `DELETE` and `SYNCHRONIZE`, so the test can remove the directory) replaces the owner's
/// implicit `READ_CONTROL`, so the owner cannot be read: the same refusal as another account's directory with a DACL
/// that hides it.
#[skuld::test]
fn a_directory_whose_owner_cannot_be_read_is_refused_naming_it(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("run");
    std::fs::create_dir(&dir).unwrap();
    set_protected_dacl(&dir, "D:P(A;;0x110000;;;OW)");
    for error in [
        WindowsHost.accept_runtime_dir(&dir).unwrap_err(),
        WindowsHost.create_runtime_dir(&dir).unwrap_err(),
    ] {
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied, "{error}");
        let text = error.to_string();
        assert!(text.contains(&dir.display().to_string()), "{text}");
        assert!(
            text.contains(&format!("cannot read the owner of {}", dir.display())),
            "{text}"
        );
    }
}

#[skuld::test]
fn file_id_of_a_junction_is_the_junction_not_its_target(#[fixture(unicode_home)] home: &Path) {
    let target = home.join("target");
    std::fs::create_dir(&target).unwrap();
    let link = home.join("junction");
    junction::create(&target, &link).unwrap();
    assert_ne!(
        WindowsHost.file_id(&link).unwrap(),
        WindowsHost.file_id(&target).unwrap()
    );
}

/// The directory is the user's alone from the moment it exists: the owner is the user, and the DACL is protected with
/// one entry, the user's.
#[skuld::test]
fn the_created_directory_is_owned_by_the_user_with_a_protected_dacl_of_one_entry(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("run");
    WindowsHost.create_runtime_dir(&dir).unwrap();
    let me = user::current_sid().unwrap();
    let security = Security::of(&dir);
    assert_eq!(security.owner(), me);
    assert!(security.dacl_is_protected());
    assert_eq!(security.allowed_sids(), vec![me]);
}
