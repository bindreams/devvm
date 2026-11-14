//! The Windows guest's own state and the host's view of it: the account, the disk, the recorded expiry, a terminal
//! session, the windows on the desktop and `doctor`'s line for the ISO.

use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use super::{
    SharedWindowsGuest, WindowsBase, printed, shared_windows_guest, windows_base, windows_iso, windows_options,
};
use crate::backend::qemu::sidecar::QemuInstall;
use crate::host;
use crate::iso::{IsoMemo, key_iso};
use crate::test_support::fixtures::iso_fixture;
use crate::test_support::qemu::{qemu_img, qemu_sidecar};
use crate::vm_suite::fixture::Up;
use crate::{Command, ExitStatus, Guest};

// The account =========================================================================================================

#[skuld::test]
fn windows_the_account_password_never_expires(#[fixture(shared_windows_guest)] guest: &SharedWindowsGuest) {
    // Autologon needs the password to hold for as long as the base does: a base outlives 42 days.
    let expires = printed(
        &guest.vm,
        r#"(Get-CimInstance Win32_UserAccount -Filter "LocalAccount = TRUE AND Name = 'user'").PasswordExpires"#,
    );
    assert_eq!(expires, "False");
}

// The disk ============================================================================================================

#[skuld::test]
fn windows_an_instance_disk_larger_than_the_base_is_claimed_by_c(
    #[fixture(windows_base)] base: &WindowsBase,
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
) {
    let up = Up::created(&base.image, "w-big-disk", &windows_options()).unwrap();
    // The base's disk is 64 GiB; this instance's is 72.
    let disk = up.dir().disk();
    let args: [&OsStr; 5] = [
        OsStr::new("resize"),
        OsStr::new("-f"),
        OsStr::new("qcow2"),
        disk.as_os_str(),
        OsStr::new("72G"),
    ];
    qemu_img(install, &args);

    up.start().unwrap();

    // The boot task extends C: before it starts sshd, so the start returns after it.
    let size: u64 = printed(&*up, "[IO.DriveInfo]::new('C').TotalSize")
        .parse()
        .expect("a size in bytes");
    assert!(size > 70 << 30, "C: is {size} bytes, not the 72 GiB disk's");
    assert!(size <= 72 << 30, "C: is {size} bytes, more than the disk");
}

// The expiry ==========================================================================================================

/// The recorded expiry is the base's license read at its build plus the grace left then; a second read in a booted
/// instance of it gives the same moment, less the time the guest was not running (its grace counts only while it runs).
/// That time is at most the time from the licence read, which came after the build began, to the second read: the
/// tolerance is measured from those two moments. Each read is in whole minutes, so each may be a minute off.
#[skuld::test]
fn windows_the_recorded_expiry_matches_an_independent_read_in_the_guest(
    #[fixture(windows_base)] base: &WindowsBase,
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    let recorded = base
        .image
        .chain
        .tip()
        .meta()
        .expires
        .expect("an evaluation base has an expiry");
    let minutes: u64 = printed(
        &guest.vm,
        r#"(Get-CimInstance SoftwareLicensingProduct -Filter "PartialProductKey IS NOT NULL AND ApplicationId='55c92734-d682-4d71-983e-d6ec3f16059f'" | Select-Object -First 1).GracePeriodRemaining"#,
    )
    .parse()
    .expect("the grace period in minutes");
    let now = SystemTime::now();

    let independent = now + Duration::from_secs(minutes * 60);

    let rounding = Duration::from_secs(2 * 60);
    let since_build_began = now.duration_since(base.build_started).unwrap_or_default();
    let (later, by) = match independent.duration_since(recorded) {
        Ok(by) => ("after", by),
        Err(e) => ("before", e.duration()),
    };
    let allowed = if later == "after" {
        since_build_began + rounding
    } else {
        rounding
    };
    assert!(
        by <= allowed,
        "the guest's own read ends {by:?} {later} the recorded expiry; at most {allowed:?} is the guest's off time"
    );
}

// doctor ==============================================================================================================

#[skuld::test]
fn windows_the_doctor_iso_line_names_a_changed_iso(
    #[fixture(windows_base)] base: &WindowsBase,
    #[fixture(windows_iso)] iso: &Path,
) {
    // A small ISO that was keyed, then changed in its last byte with its size and time kept: only hashing it again
    // notices. The line reads the key's file whatever its size. The directory goes with the test, whatever the result;
    // the ISO the base was built from is left alone.
    let dir = tempfile::tempdir_in(&base.roots.dirs.state).unwrap();
    let changed = dir.path().join("changed.iso");
    std::fs::copy(iso_fixture("single-image.iso"), &changed).unwrap();
    let memo = IsoMemo::new(&base.roots.dirs);
    drop(key_iso(host::current(), &memo, &changed, None, &*base.progress).unwrap());
    {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&changed)
            .unwrap();
        let mtime = file.metadata().unwrap().modified().unwrap();
        file.seek(SeekFrom::End(-1)).unwrap();
        let mut last = [0_u8];
        file.read_exact(&mut last).unwrap();
        file.seek(SeekFrom::End(-1)).unwrap();
        file.write_all(&[last[0] ^ 0xff]).unwrap();
        file.set_modified(mtime).unwrap();
    }

    let checks = base.engine.checks();

    let line = |path: &Path| {
        checks
            .iter()
            .find(|check| check.name == format!("iso {}", path.display()))
            .unwrap_or_else(|| panic!("no line for {}: {checks:?}", path.display()))
    };
    let why = line(&changed).outcome.clone().unwrap_err();
    assert!(why.contains("changed since it was keyed"), "{why}");
    let hash = line(iso).outcome.clone().unwrap();
    assert!(
        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "the line of the base's ISO: {hash}"
    );
}

// A terminal ==========================================================================================================

/// Everything `ssh -tt` printed, a chunk at a time, until it ends.
fn output_of(stdout: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdout = stdout;
        let mut chunk = [0_u8; 4096];
        while let Ok(read) = stdout.read(&mut chunk) {
            if read == 0 || sender.send(chunk[..read].to_vec()).is_err() {
                break;
            }
        }
    });
    receiver
}

/// An interactive session through the system `ssh -tt`: a command typed into it runs and prints, and a Ctrl-C byte ends a
/// running command. The input stays open until the last marker has come back, then the shell exits and the input closes:
/// a terminal session whose input ends first may end without running anything (the stated limit), and the status of an
/// interactive session is 0 whatever happened (another), so the test asserts neither.
///
/// `devvm ssh <name>` itself passes no `-tt`; this is the system client's, with the generated configuration.
#[skuld::test]
fn windows_interactive_ssh_runs_a_command_and_returns_its_output(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    let config = guest.vm.instance().ssh_config().unwrap();
    let mut command = std::process::Command::new("ssh");
    command
        .arg("-tt")
        .arg("-F")
        .arg(&config)
        .args(["-o", "BatchMode=yes"])
        .arg(guest.vm.name())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    host::current().quiet(&mut command);
    let mut ssh = command.spawn().unwrap_or_else(|e| panic!("cannot run ssh: {e}"));
    let mut input = ssh.stdin.take().expect("a piped stdin");
    let output = output_of(ssh.stdout.take().expect("a piped stdout"));
    let mut seen = String::new();
    // Waits for `needle` in what the terminal printed. The commands print it joined, and the terminal's echo of what was
    // typed has it in pieces, so only a command that ran can satisfy it.
    let mut await_output = |needle: &str| {
        while !seen.contains(needle) {
            match output.recv() {
                Ok(chunk) => seen.push_str(&String::from_utf8_lossy(&chunk)),
                Err(_) => panic!("ssh ended before it printed {needle:?}; it printed {seen:?}"),
            }
        }
    };

    input.write_all(b"Write-Output ('devvm-' + 'marker')\r").unwrap();
    await_output("devvm-marker");

    input
        .write_all(b"'run-' + 'ning'; while ($true) { Start-Sleep -Milliseconds 100 }\r")
        .unwrap();
    await_output("run-ning");
    // Ctrl-C ends the loop, and the shell takes the next command.
    input.write_all(&[0x03]).unwrap();
    input.write_all(b"'af-' + 'ter'\r").unwrap();
    await_output("af-ter");

    input.write_all(b"exit\r").unwrap();
    drop(input);
    ssh.wait().unwrap();
}

// The desktop =========================================================================================================

/// The names of the programs that own a visible top-level window on the desktop the script runs on.
const VISIBLE_WINDOWS: &str = r#"Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class DevvmWindows {
    private delegate bool EnumProc(IntPtr window, IntPtr state);
    [DllImport("user32.dll")] private static extern bool EnumWindows(EnumProc proc, IntPtr state);
    [DllImport("user32.dll")] private static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr window, out uint pid);
    public static uint[] Visible() {
        var owners = new List<uint>();
        EnumWindows((window, state) => {
            if (IsWindowVisible(window)) { uint pid; GetWindowThreadProcessId(window, out pid); owners.Add(pid); }
            return true;
        }, IntPtr.Zero);
        return owners.ToArray();
    }
}
'@
foreach ($id in ([DevvmWindows]::Visible() | Sort-Object -Unique)) {
    $process = Get-Process -Id $id -ErrorAction Ignore
    if ($process) { $process.ProcessName }
}"#;

/// The programs a command of devvm runs in, which must never show a window: the agent and the shells and consoles it
/// starts, Windows Terminal (the default terminal of 25H2) included.
const NO_WINDOW: [&str; 8] = [
    "ssh-broker",
    "WindowsTerminal",
    "OpenConsole",
    "conhost",
    "cmd",
    "pwsh",
    "powershell",
    "wt",
];

#[skuld::test]
fn windows_the_agent_has_no_visible_window(#[fixture(shared_windows_guest)] guest: &SharedWindowsGuest) {
    // Programs of each kind have run in the agent's session by now; the ones of this test run first.
    for argv in [
        ["cmd", "/d", "/c", "ver"],
        ["pwsh", "-NoProfile", "-NonInteractive", "-Command"],
    ] {
        let mut argv: Vec<&str> = argv.to_vec();
        if argv[0] == "pwsh" {
            argv.push("1");
        }
        let seen = guest.vm.run(&Command::new(argv)).unwrap();
        assert_eq!(seen.status, ExitStatus::Code(0), "{}", seen.stderr_lossy());
    }

    let owners = printed(&guest.vm, VISIBLE_WINDOWS);

    let shown: Vec<&str> = owners
        .lines()
        .map(str::trim)
        .filter(|name| NO_WINDOW.iter().any(|program| program.eq_ignore_ascii_case(name)))
        .collect();
    assert_eq!(shown, Vec::<&str>::new(), "visible windows of {owners:?}");
}
