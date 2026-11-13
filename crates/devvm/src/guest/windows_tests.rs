use crate::ErrorKind;
use crate::backend::{DeviceProfile, DiskBus, NetModel, Video};
use crate::guest::GuestOs;
use crate::guest::windows::WindowsGuest;
use crate::recipe::{Setting, Shell, UacMode};
use crate::{Command, RunAs};

#[skuld::test]
fn boot_id_command_reads_the_boot_task_file_and_needs_no_elevation() {
    let command = WindowsGuest.boot_id_command();
    assert_eq!(command.run_as, RunAs::User);
    assert_eq!(command.argv.last().unwrap(), r"type C:\ProgramData\devvm\boot-id");
    // It runs through the wrapper like any command, so it is answered from the user's session.
    assert!(WindowsGuest.wrap(&command).is_ok());
}

#[skuld::test]
fn flush_runs_unelevated() {
    let command = WindowsGuest.flush_command();
    assert_eq!(command.run_as, RunAs::User);
    assert_eq!(command.argv[0], "pwsh");
    assert!(
        command
            .argv
            .last()
            .unwrap()
            .contains("Write-VolumeCache -DriveLetter C")
    );
}

#[skuld::test]
fn shell_argv_for_pwsh_writes_the_output_as_utf8_first_and_keeps_the_script_its_own_block() {
    let argv = WindowsGuest.shell_argv(Some(Shell::Pwsh), "Write-Output 'hi'").unwrap();
    assert_eq!(argv[..4], ["pwsh", "-NoProfile", "-NonInteractive", "-Command"]);
    // The script is a literal of its own: a first statement that must come first (`using`, `param`) still does.
    assert_eq!(
        argv[4],
        "[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); & ([scriptblock]::Create('Write-Output ''hi'''))"
    );
    // The guest's default shell is the same one.
    assert_eq!(WindowsGuest.shell_argv(None, "Write-Output 'hi'").unwrap(), argv);
}

#[skuld::test]
fn shell_argv_for_cmd_is_a_c_command() {
    assert_eq!(
        WindowsGuest
            .shell_argv(Some(Shell::Cmd), "echo hi & echo there")
            .unwrap(),
        ["cmd", "/d", "/c", "echo hi & echo there"]
    );
}

#[skuld::test]
fn shell_argv_for_sh_and_bash_is_a_recipe_error_naming_the_guest() {
    for shell in [Shell::Sh, Shell::Bash] {
        let error = WindowsGuest.shell_argv(Some(shell), "echo hi").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Recipe);
        for word in ["windows", "pwsh", "cmd"] {
            assert!(error.message().contains(word), "{word}: {error}");
        }
    }
}

#[skuld::test]
fn uac_auto_approve_is_an_elevated_registry_write() {
    let command = WindowsGuest
        .setting_command(&Setting::Uac(UacMode::AutoApprove))
        .unwrap();
    assert_eq!(command.run_as, RunAs::Elevated);
    assert_eq!(command.cwd, None);
    let script = command.argv.last().unwrap();
    assert!(
        script.contains(r"HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System")
            && script.contains("ConsentPromptBehaviorAdmin")
            && script.contains("-Value 0")
            && script.contains("DWord"),
        "{script}"
    );
    assert!(WindowsGuest.wrap(&command).is_ok());
}

#[skuld::test]
fn the_devices_are_virtio_with_a_vga_display_and_a_guest_that_reboots_inside_its_process() {
    assert_eq!(
        WindowsGuest.device_profile(),
        DeviceProfile {
            disk: DiskBus::Virtio,
            net: NetModel::Virtio,
            video: Video::Vga,
            no_reboot: false,
        }
    );
    assert_eq!(WindowsGuest.semantics_version(), 1);
}

#[skuld::test]
fn a_file_name_follows_the_rules_of_windows() {
    assert!(WindowsGuest.file_name_problem("a?b").is_some());
    assert!(WindowsGuest.file_name_problem("a:b").is_some());
    assert!(WindowsGuest.file_name_problem("fine.txt").is_none());
}

#[skuld::test]
fn a_command_is_wrapped_by_the_windows_wrapper() {
    let text = WindowsGuest.wrap(&Command::new(["tool"])).unwrap();
    assert!(text.contains("$app = Find-Program 'tool'"), "{text}");
}

#[skuld::test]
fn a_cmd_step_script_with_a_block_scalars_line_end_is_wrapped_to_run_and_is_not_refused() {
    let argv = WindowsGuest.shell_argv(Some(Shell::Cmd), "echo a\r\n").unwrap();
    let text = WindowsGuest.wrap(&Command::new(argv)).unwrap();
    assert!(text.contains(r#"$psi.Arguments = '/d /c "echo a"'"#), "{text}");
    // What runs for `cmd.exe` is not a refusal.
    let cmd_branch = &text[text.find("elseif").unwrap()..];
    let cmd_branch = &cmd_branch[..cmd_branch.find("} else {").unwrap()];
    assert!(!cmd_branch.contains("exit 126"), "{text}");
}

#[skuld::test]
fn shell_argv_for_cmd_drops_the_line_end_a_block_scalar_leaves() {
    for script in ["echo hi\n", "echo hi\r\n", "echo hi\n\n"] {
        assert_eq!(
            WindowsGuest.shell_argv(Some(Shell::Cmd), script).unwrap(),
            ["cmd", "/d", "/c", "echo hi"],
            "{script:?}"
        );
    }
}

#[skuld::test]
fn shell_argv_for_cmd_refuses_several_lines_naming_the_limit_and_the_way_out() {
    for script in ["echo a\necho b", "echo a\r\necho b\n", "echo a\recho b"] {
        let error = WindowsGuest.shell_argv(Some(Shell::Cmd), script).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Recipe, "{script:?}");
        for word in ["cmd", "one line", "shell: pwsh"] {
            assert!(error.message().contains(word), "{word}: {error}");
        }
        // A `copy` step cannot bring in a file on this guest, so none is advised.
        assert!(!error.message().contains("copy"), "{error}");
    }
}
