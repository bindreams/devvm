//! What a command does in the Windows guest: the exec table of `exec/cases.rs` (exit codes, arguments, directories), `cp`
//! with drive paths, and the length of a script. These change nothing in the guest, so they share one instance.

use std::path::{Path, PathBuf};

use super::windows_recipe;
use super::{
    SharedWindowsGuest, WindowsBase, printed, shared_windows_guest, windows_base, windows_iso, windows_options,
};
use crate::access::sftp::sftp_tests::tree;
use crate::guest::GuestOs;
use crate::guest::windows::WindowsGuest;
use crate::test_support::fixtures::unicode_home;
use crate::vm_suite::exec::InGuest;
use crate::vm_suite::exec::cases::{self, Os};
use crate::vm_suite::fixture::Up;
use crate::{BuildOptions, Command, ErrorKind, ExitStatus, Guest};

#[skuld::test]
fn windows_exec_exit_codes_7_125_255_and_ntstatus(#[fixture(shared_windows_guest)] guest: &SharedWindowsGuest) {
    cases::exit_7_125_255(&InGuest(&guest.vm, Os::Windows));
}

#[skuld::test]
fn windows_a_guest_printing_devvm_error_and_exiting_125_is_a_guest_status(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    cases::a_guest_printing_devvm_error_and_exiting_125_is_a_guest_status(&InGuest(&guest.vm, Os::Windows));
}

#[skuld::test]
fn windows_argv_round_trips_hostile_arguments(#[fixture(shared_windows_guest)] guest: &SharedWindowsGuest) {
    cases::argv_round_trips_hostile_arguments(&InGuest(&guest.vm, Os::Windows));
}

#[skuld::test]
fn windows_cwd_and_elevated_cwd_are_honored(#[fixture(shared_windows_guest)] guest: &SharedWindowsGuest) {
    cases::cwd_is_honored(&InGuest(&guest.vm, Os::Windows));
    cases::elevated_cwd_is_honored(&InGuest(&guest.vm, Os::Windows));
}

// cp ==================================================================================================================

/// A tree with a space, non-ASCII characters and a surrogate pair in its names. It holds no link: the user's token
/// cannot make one in the guest.
fn pack(home: &Path) -> PathBuf {
    let pack = home.join("pack");
    std::fs::create_dir_all(pack.join("sub dir")).unwrap();
    std::fs::write(pack.join("real.txt"), b"real").unwrap();
    std::fs::write(pack.join("\u{e9} \u{416} \u{4e16} \u{1f600}.txt"), b"unicode").unwrap();
    std::fs::write(pack.join("sub dir").join("nested.txt"), b"nested").unwrap();
    pack
}

#[skuld::test]
fn windows_cp_files_and_directories_both_ways_with_drive_paths(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
    #[fixture(unicode_home)] home: &Path,
) {
    let pack = pack(home);
    let file = home.join("one.txt");
    std::fs::write(&file, b"one").unwrap();

    // A drive path with forward slashes, the spelling `cp` takes, for a tree and for a file.
    guest.vm.copy_in(&pack, "C:/ProgramData/devvm/cp-pack").unwrap();
    guest.vm.copy_in(&file, "C:/ProgramData/devvm/cp-one.txt").unwrap();

    let listed = printed(
        &guest.vm,
        "Get-ChildItem -LiteralPath 'C:\\ProgramData\\devvm\\cp-pack' -Recurse | ForEach-Object { \
         $_.FullName.Substring('C:\\ProgramData\\devvm\\cp-pack\\'.Length) + $(if ($_.PSIsContainer) { '/' } else { '' }) } | Sort-Object",
    );
    let mut guest_tree: Vec<String> = listed.lines().map(|line| line.trim().replace('\\', "/")).collect();
    guest_tree.sort();
    assert_eq!(guest_tree, tree(&pack), "the tree in the guest");
    assert_eq!(
        printed(
            &guest.vm,
            "[IO.File]::ReadAllText('C:\\ProgramData\\devvm\\cp-pack\\sub dir\\nested.txt') + \
             [IO.File]::ReadAllText('C:\\ProgramData\\devvm\\cp-one.txt')"
        ),
        "nestedone"
    );

    guest
        .vm
        .copy_out("C:/ProgramData/devvm/cp-pack", &home.join("back"))
        .unwrap();
    guest
        .vm
        .copy_out("C:/ProgramData/devvm/cp-one.txt", &home.join("one-back.txt"))
        .unwrap();

    assert_eq!(tree(&home.join("back")), tree(&pack), "the tree back");
    assert_eq!(
        std::fs::read(home.join("back").join("sub dir").join("nested.txt")).unwrap(),
        b"nested"
    );
    assert_eq!(
        std::fs::read(home.join("back").join("\u{e9} \u{416} \u{4e16} \u{1f600}.txt")).unwrap(),
        b"unicode"
    );
    assert_eq!(std::fs::read(home.join("one-back.txt")).unwrap(), b"one");
}

// pwsh ================================================================================================================

#[skuld::test]
fn windows_a_pwsh_argv_and_the_default_shell_step_resolve_pwsh_by_name(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
    #[fixture(windows_base)] base: &WindowsBase,
    #[fixture(windows_iso)] iso: &Path,
) {
    // That an argv naming `pwsh` runs it is `pwsh_resolves_by_name_in_the_built_base`'s; this is the PATH it finds it on.
    assert_eq!(
        printed(
            &guest.vm,
            r"[Environment]::GetEnvironmentVariable('Path', 'Machine') -split ';' -contains 'C:\Program Files\PowerShell\7'"
        ),
        "True",
        "the machine PATH names PowerShell 7's directory"
    );

    // A step with no `shell` runs in `pwsh`, which the step's own guest finds by name.
    let recipe = windows_recipe(
        &base.roots.dirs.state,
        iso,
        &[
            "run: 'Set-Content -LiteralPath C:/ProgramData/devvm/default-shell.txt -Value $PSVersionTable.PSVersion.Major'",
        ],
    );
    let image = base.engine.build(&recipe, &BuildOptions::default()).unwrap();
    let up = Up::new(&image, "w-default-shell", &windows_options()).unwrap();
    assert_eq!(
        printed(&*up, "Get-Content -LiteralPath C:/ProgramData/devvm/default-shell.txt"),
        "7"
    );
}

// Length ==============================================================================================================

/// A command whose script is `'ok'` and a comment of `pad` characters.
fn padded(pad: usize) -> Command {
    Command::new([
        "pwsh".to_owned(),
        "-NoProfile".to_owned(),
        "-NonInteractive".to_owned(),
        "-Command".to_owned(),
        format!("'ok' # {}", "a".repeat(pad)),
    ])
}

/// The longest script that `wrap` takes runs in the guest, and the next one is refused before anything is sent. `wrap`'s
/// limit is computed from both command lines of the guest (sshd's and ssh-broker's), so a script that it takes and the
/// guest cannot run means the computation is wrong.
#[skuld::test]
fn windows_a_script_at_the_computed_limit_runs_and_one_over_is_refused_before_sending(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    // `wrap` takes a longer script only while the lines stay under the limit, so the longest pad is found by halving.
    let (mut fits, mut over) = (0_usize, 40_000_usize);
    WindowsGuest
        .wrap(&padded(fits))
        .expect("an empty pad is within the limit");
    WindowsGuest
        .wrap(&padded(over))
        .expect_err("a pad of 40,000 is over the limit");
    while over - fits > 1 {
        let middle = fits + (over - fits) / 2;
        if WindowsGuest.wrap(&padded(middle)).is_ok() {
            fits = middle;
        } else {
            over = middle;
        }
    }

    let at_the_limit = guest.vm.run(&padded(fits)).unwrap();
    assert_eq!(
        at_the_limit.status,
        ExitStatus::Code(0),
        "a pad of {fits}: {}",
        at_the_limit.stderr_lossy()
    );
    assert_eq!(at_the_limit.stdout_lossy().trim(), "ok", "a pad of {fits}");

    let error = guest.vm.run(&padded(over)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Usage, "a pad of {over}: {error}");
}
