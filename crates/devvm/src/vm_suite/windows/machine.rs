//! The Windows guest's own state and the host's view of it: the account, the disk, the recorded expiry, a terminal
//! session, the windows on the desktop and `doctor`'s line for the ISO.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

use super::bounds::TERMINAL_BOUND;
use super::{
    SharedWindowsGuest, WindowsBase, printed, pwsh, ran, shared_windows_guest, windows_base, windows_iso,
    windows_options,
};
use crate::RunAs;
use crate::backend::qemu::sidecar::QemuInstall;
use crate::host;
use crate::iso::names_an_evaluation;
use crate::test_support::qemu::{qemu_img, qemu_sidecar};
use crate::vm_suite::fixture::Up;
use crate::vm_suite::waits::{awaiting, step};

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

/// The disk of C: and its partitions, in bytes, and how far Windows says C: can grow.
const LAYOUT: &str = r#"$c = Get-Partition -DriveLetter C; "disk $((Get-Disk -Number $c.DiskNumber).Size)"; Get-Partition -DiskNumber $c.DiskNumber | ForEach-Object { "partition $($_.PartitionNumber) $($_.Type) offset $($_.Offset) size $($_.Size)" }; "C: can grow to $((Get-PartitionSupportedSize -DriveLetter C).SizeMax)""#;

#[skuld::test]
fn windows_an_instance_disk_larger_than_the_base_is_claimed_by_c(
    #[fixture(windows_base)] base: &WindowsBase,
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
) {
    step("making an instance and growing its disk");
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

    step("starting it");
    up.start().unwrap();

    // The boot task extends C: before it starts sshd, so the start returns after it.
    let size: u64 = printed(&*up, "[IO.DriveInfo]::new('C').TotalSize")
        .parse()
        .expect("a size in bytes");
    // What the guest says of the disk's layout, for a C: that did not grow: the look is not allowed to fail the message.
    let layout = || match ran(&*up, "a look at the disk", &pwsh(RunAs::Elevated, LAYOUT)) {
        Ok(output) => format!("{}{}", output.stdout_lossy(), output.stderr_lossy()),
        Err(error) => format!("the look at the disk failed: {error}"),
    };
    assert!(
        size > 70 << 30,
        "C: is {size} bytes, not the 72 GiB disk's; the guest says:\n{}",
        layout()
    );
    assert!(size <= 72 << 30, "C: is {size} bytes, more than the disk");
}

// The CPU =============================================================================================================

/// Defines `[Cpu]::Id(leaf, sub)`, which runs CPUID in the guest, and `[Cpu]::Xcr(index)`, which runs XGETBV.
const CPU: &str = r#"Add-Type -TypeDefinition '
using System;
using System.Runtime.InteropServices;
public static class Cpu {
  [DllImport("kernel32.dll")] static extern IntPtr VirtualAlloc(IntPtr a, UIntPtr s, uint t, uint p);
  [UnmanagedFunctionPointer(CallingConvention.Cdecl)] delegate void CpuidFn(uint leaf, uint sub, IntPtr result);
  [UnmanagedFunctionPointer(CallingConvention.Cdecl)] delegate ulong XgetbvFn(uint index);
  static IntPtr Code(byte[] bytes) {
    IntPtr p = VirtualAlloc(IntPtr.Zero, (UIntPtr)4096, 0x3000, 0x40);
    Marshal.Copy(bytes, 0, p, bytes.Length);
    return p;
  }
  // mov eax,ecx; mov ecx,edx; cpuid; store eax, ebx, ecx, edx at r8.
  static CpuidFn cpuid = (CpuidFn)Marshal.GetDelegateForFunctionPointer(Code(new byte[] { 0x53, 0x89, 0xC8, 0x89, 0xD1, 0x0F, 0xA2, 0x41, 0x89, 0x00, 0x41, 0x89, 0x58, 0x04, 0x41, 0x89, 0x48, 0x08, 0x41, 0x89, 0x50, 0x0C, 0x5B, 0xC3 }), typeof(CpuidFn));
  // xgetbv; rax = edx:eax.
  static XgetbvFn xgetbv = (XgetbvFn)Marshal.GetDelegateForFunctionPointer(Code(new byte[] { 0x0F, 0x01, 0xD0, 0x48, 0xC1, 0xE2, 0x20, 0x48, 0x09, 0xD0, 0xC3 }), typeof(XgetbvFn));
  public static uint[] Id(uint leaf, uint sub) {
    IntPtr memory = Marshal.AllocHGlobal(16);
    cpuid(leaf, sub, memory);
    int[] registers = new int[4];
    Marshal.Copy(memory, registers, 0, 4);
    Marshal.FreeHGlobal(memory);
    return new uint[] { (uint)registers[0], (uint)registers[1], (uint)registers[2], (uint)registers[3] };
  }
  public static ulong Xcr(uint index) { return xgetbv(index); }
}'"#;

/// Prints `xsave=0` for a CPU without XSAVE, or `xcr0=<hex> needed=<bytes> reported=<bytes>`: the area the enabled
/// XSAVE components fill, and the size CPUID leaf 0xD says it is.
const XSAVE_AREA: &str = r#"if (([Cpu]::Id(1, 0)[2] -band 0x04000000) -eq 0) { 'xsave=0'; return }
$xcr0 = [Cpu]::Xcr(0)
$needed = 0x240
foreach ($component in 2..62) {
  if (($xcr0 -shr $component) -band 1) {
    $r = [Cpu]::Id(13, $component)
    $needed = [Math]::Max($needed, [int]($r[1] + $r[0]))
  }
}
$reported = [Cpu]::Id(13, 0)[1]
"xcr0=0x{0:X} needed=$needed reported=$reported" -f $xcr0"#;

/// Cygwin 3.6 and later (Git Bash) sizes the buffer of the `xsave64` it runs on every signal, which includes the ones a
/// fork raises, by CPUID leaf 0xD, sub-leaf 0, EBX. Under WHPX, QEMU answers that from an XCR0 it never reads back from
/// the guest, so a CPU with AVX reports 0x240 bytes for an area that fills 0x340 or more, and bash crashes on its first
/// fork.
#[skuld::test]
fn windows_the_xsave_area_cpuid_reports_holds_the_enabled_features(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    let report = printed(&guest.vm, &format!("{CPU}\n{XSAVE_AREA}"));
    if report == "xsave=0" {
        return;
    }
    let (needed, reported) = (number(&report, "needed="), number(&report, "reported="));
    assert!(
        reported >= needed,
        "CPUID says the XSAVE area is {reported} bytes; the enabled features fill {needed} ({report})"
    );
}

/// Prints `vendor=<name>` when the hypervisor is not Microsoft's, `max=<number>` when its highest leaf is below
/// 0x4000000A, else `max=<number> eax=<number>`: the highest hypervisor leaf, and the EAX of leaf 0x4000000A.
const ENLIGHTENED_NPT_TLB: &str = r#"$hv = [Cpu]::Id(0x40000000, 0)
$vendor = [Text.Encoding]::ASCII.GetString([BitConverter]::GetBytes($hv[1]) + [BitConverter]::GetBytes($hv[2]) + [BitConverter]::GetBytes($hv[3]))
if ($vendor -ne 'Microsoft Hv') { "vendor=$vendor"; return }
if ($hv[0] -lt 0x4000000A) { "max=$($hv[0])"; return }
"max=$($hv[0]) eax=$([Cpu]::Id(0x4000000A, 0)[0])""#;

/// Bit 22 of CPUID leaf 0x4000000A, EAX, promises the enlightened NPT TLB: the guest may flush its nested page tables
/// by hypercall. The hypervisor rejects that hypercall for a QEMU guest, so a hypervisor inside the guest that trusts
/// the bit runs its VMs on stale nested page tables and they crash.
#[skuld::test]
fn windows_the_guest_is_not_shown_the_enlightened_npt_tlb(#[fixture(shared_windows_guest)] guest: &SharedWindowsGuest) {
    let report = printed(&guest.vm, &format!("{CPU}\n{ENLIGHTENED_NPT_TLB}"));
    assert!(
        report.starts_with("max="),
        "the guest's hypervisor is not Microsoft's, which WHPX's is: {report}"
    );
    if !report.contains(" eax=") {
        eprintln!("not checked: the hypervisor's highest leaf is below 0x4000000A ({report})");
        return;
    }
    let eax = number(&report, "eax=");
    assert_eq!(
        eax & (1 << 22),
        0,
        "CPUID leaf 0x4000000A, EAX bit 22 (the enlightened NPT TLB) is set: EAX is {eax:#x} ({report})"
    );
}

fn number(report: &str, field: &str) -> u32 {
    report
        .split_whitespace()
        .find_map(|word| word.strip_prefix(field))
        .and_then(|digits| digits.parse().ok())
        .unwrap_or_else(|| panic!("no {field}<number> in the guest's answer: {report:?}"))
}

// The expiry ==========================================================================================================

/// The recorded expiry is the base's license read at its build plus the grace left then; a second read in a booted
/// instance of it gives the same moment, less the time the guest was not running (its grace counts only while it runs).
/// That time is at most the time from the build's license read, which the build reports as it arrives, to the second
/// read: the tolerance is measured from those two moments. Each read is in whole minutes, so each may be a minute off.
/// A base of an edition that is not an evaluation has no expiry, and the guest's own read agrees: its license is not an
/// evaluation family's.
#[skuld::test]
fn windows_the_recorded_expiry_matches_an_independent_read_in_the_guest(
    #[fixture(windows_base)] base: &WindowsBase,
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    let expires = base.image.chain.tip().meta().expires;
    if !base.evaluation {
        assert_eq!(expires, None, "a base of a non-evaluation edition has no expiry");
        let read = |property: &str| {
            printed(
                &guest.vm,
                &format!(
                    r#"(Get-CimInstance SoftwareLicensingProduct -Filter "PartialProductKey IS NOT NULL AND ApplicationId='55c92734-d682-4d71-983e-d6ec3f16059f'" | Select-Object -First 1).{property}"#
                ),
            )
        };
        let family = read("LicenseFamily");
        assert!(
            !family.is_empty() && !names_an_evaluation(&family),
            "the guest reads the license family `{family}`, which is an evaluation's or none"
        );
        // The guest keeps only the last five characters of the installed key.
        let key = base
            .product_key
            .as_ref()
            .expect("a non-evaluation base is installed with a key");
        let tail = &key.expose()[key.expose().len() - 5..];
        assert_eq!(
            read("PartialProductKey"),
            tail,
            "the guest's installed key ends in the build's key's tail"
        );
        return;
    }
    let recorded = expires.expect("an evaluation base has an expiry");
    let minutes: u64 = printed(
        &guest.vm,
        r#"(Get-CimInstance SoftwareLicensingProduct -Filter "PartialProductKey IS NOT NULL AND ApplicationId='55c92734-d682-4d71-983e-d6ec3f16059f'" | Select-Object -First 1).GracePeriodRemaining"#,
    )
    .parse()
    .expect("the grace period in minutes");
    let now = SystemTime::now();
    let read_at = base.license_read_at;

    let independent = now + Duration::from_secs(minutes * 60);

    let rounding = Duration::from_secs(2 * 60);
    let since_read = now.duration_since(read_at).unwrap_or_default();
    let (later, by) = match independent.duration_since(recorded) {
        Ok(by) => ("after", by),
        Err(e) => ("before", e.duration()),
    };
    let allowed = if later == "after" {
        since_read + rounding
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
fn windows_the_doctor_iso_line_of_the_base_iso_shows_a_sha256(
    #[fixture(windows_base)] base: &WindowsBase,
    #[fixture(windows_iso)] iso: &Path,
) {
    let checks = base.engine.checks();

    let hash = checks
        .iter()
        .find(|check| check.name == format!("iso {}", iso.display()))
        .unwrap_or_else(|| panic!("no line for {}: {checks:?}", iso.display()))
        .outcome
        .clone()
        .unwrap();
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

/// The `ssh` client of a test, ended when the test is: a test that fails leaves no session running on the guest.
pub(super) struct Client(pub(super) std::process::Child);

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// An interactive session through the system `ssh -tt`: a typed command runs and prints, and Ctrl-C ends a running one.
/// Each step is typed after the prompt before it, since a console drops keys typed during a cancel. The input closes only
/// after the last prompt (README limits), and the status is not asserted.
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
    let mut ssh = Client(command.spawn().unwrap_or_else(|e| panic!("cannot run ssh: {e}")));
    let mut input = ssh.0.stdin.take().expect("a piped stdin");
    let output = output_of(ssh.0.stdout.take().expect("a piped stdout"));
    let mut seen = String::new();
    // Waits for `needle` in what the terminal printed, which `after` makes happen. A needle is text that only a command
    // that ran or a prompt that was printed produces, never the terminal's echo of what was typed: the commands print
    // their marker joined, and the prompt definition is typed as `"p$(...)-ready> "`, which has no `p1-ready`. A
    // terminal that has not printed it within [`TERMINAL_BOUND`] of the call fails the test, however much else it printed.
    let mut await_output = |needle: &str, after: &str| {
        step(&format!("awaiting {needle:?} in the terminal, after {after}"));
        let deadline = Instant::now() + TERMINAL_BOUND;
        while !seen.contains(needle) {
            // A terminal that keeps printing never times out a receive, so the deadline is checked before each one.
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                panic!("ssh did not print {needle:?} within {TERMINAL_BOUND:?}, after {after}; it printed {seen:?}");
            }
            match output.recv_timeout(left) {
                Ok(chunk) => seen.push_str(&String::from_utf8_lossy(&chunk)),
                Err(RecvTimeoutError::Timeout) => {
                    panic!("ssh did not print {needle:?} within {TERMINAL_BOUND:?}, after {after}; it printed {seen:?}")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("ssh ended before it printed {needle:?}, after {after}; it printed {seen:?}")
                }
            }
        }
    };
    // The prompt the session prints for the `n`th time after its first command. The session's own first prompt is
    // printed before the function exists, so the numbering starts with the prompt that follows the first command.
    // The needle stops before the `> `, which the line editor repaints in its own colour as a line is typed.
    let prompt = |n: usize| format!("p{n}-ready");

    input
        .write_all(
            b"$global:devvmPrompt = 0; function prompt { $global:devvmPrompt++; \"p$($global:devvmPrompt)-ready> \" }; \
              Write-Output ('devvm-' + 'marker')\r",
        )
        .unwrap();
    await_output("devvm-marker", "the typed command ran");
    await_output(&prompt(1), "the first command ended");

    // The loop has no bound: only the Ctrl-C ends it, and a Ctrl-C that does not fails the wait for the next prompt.
    input
        .write_all(b"'run-' + 'ning'; while ($true) { Start-Sleep -Milliseconds 100 }\r")
        .unwrap();
    await_output("run-ning", "the loop started");
    // Ctrl-C ends the loop, and the prompt that follows is the second. Keys typed before it are dropped.
    input.write_all(&[0x03]).unwrap();
    await_output(&prompt(2), "Ctrl-C ended the loop");
    input.write_all(b"'af-' + 'ter'\r").unwrap();
    await_output("af-ter", "the command after Ctrl-C ran");
    await_output(&prompt(3), "the command after Ctrl-C ended");

    input.write_all(b"exit\r").unwrap();
    drop(input);
    let _watch = awaiting("the interactive ssh ending", TERMINAL_BOUND);
    ssh.0.wait().unwrap();
}

// The desktop =========================================================================================================

/// The names of the programs that own a visible top-level window on the desktop the script runs on.
const VISIBLE_WINDOWS: &str = r#"$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
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
    // Each listing runs while its program does: pwsh lists the windows itself, and cmd is the parent of the pwsh that
    // does. A window that closes with its program is gone from a later listing.
    let file = "C:/ProgramData/devvm/visible-windows.ps1";
    let by_cmd = printed(
        &guest.vm,
        &format!(
            "$ErrorActionPreference = 'Stop'; $script = {{ {VISIBLE_WINDOWS} }}; Set-Content -LiteralPath {file} -Value $script.ToString(); \
             cmd /d /c \"pwsh -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {file}\"; $listed = $LASTEXITCODE; \
             Remove-Item -LiteralPath {file}; exit $listed"
        ),
    );
    let by_pwsh = printed(&guest.vm, VISIBLE_WINDOWS);
    let owners = [by_pwsh, by_cmd].join("\n");

    let shown: Vec<&str> = owners
        .lines()
        .map(str::trim)
        .filter(|name| NO_WINDOW.iter().any(|program| program.eq_ignore_ascii_case(name)))
        .collect();
    assert_eq!(shown, Vec::<&str>::new(), "visible windows of {owners:?}");
}
