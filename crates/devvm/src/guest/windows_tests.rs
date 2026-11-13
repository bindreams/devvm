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
fn shell_argv_for_pwsh_writes_the_output_as_utf8_first() {
    let argv = WindowsGuest.shell_argv(Some(Shell::Pwsh), "Write-Output hi").unwrap();
    assert_eq!(argv[..4], ["pwsh", "-NoProfile", "-NonInteractive", "-Command"]);
    assert_eq!(
        argv[4],
        "[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); Write-Output hi"
    );
    // The guest's default shell is the same one.
    assert_eq!(WindowsGuest.shell_argv(None, "Write-Output hi").unwrap(), argv);
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
    assert!(text.contains("Get-Command -CommandType Application"), "{text}");
}
