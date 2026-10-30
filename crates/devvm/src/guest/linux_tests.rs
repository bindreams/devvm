use crate::backend::{DeviceProfile, DiskBus, NetModel};
use crate::guest::linux::LinuxGuest;
use crate::recipe::Shell;
use crate::{Command, ErrorKind};

#[skuld::test]
fn shell_argv_cases() {
    let argv = |shell, script| LinuxGuest.shell_argv(shell, script).unwrap();
    assert_eq!(argv(None, "echo hi"), ["bash", "-c", "echo hi"]);
    assert_eq!(argv(Some(Shell::Bash), "echo hi"), ["bash", "-c", "echo hi"]);
    assert_eq!(argv(Some(Shell::Sh), "echo hi"), ["sh", "-c", "echo hi"]);
    assert_eq!(
        argv(Some(Shell::Pwsh), "Write-Output hi"),
        ["pwsh", "-NoProfile", "-Command", "Write-Output hi"]
    );
    let error = LinuxGuest.shell_argv(Some(Shell::Cmd), "dir").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("cmd"), "{}", error.message());
}

#[skuld::test]
fn the_boot_id_is_the_kernels() {
    let Command { argv, .. } = LinuxGuest.boot_id_command();
    assert_eq!(argv, ["cat", "/proc/sys/kernel/random/boot_id"]);
}

#[skuld::test]
fn semantics_version_is_one() {
    assert_eq!(LinuxGuest.semantics_version(), 1);
}

#[skuld::test]
fn virtio_devices_and_a_guest_that_reboots_inside_its_process() {
    assert_eq!(
        LinuxGuest.device_profile(),
        DeviceProfile {
            disk: DiskBus::Virtio,
            net: NetModel::Virtio,
            no_reboot: false,
        }
    );
}
