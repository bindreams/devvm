use quick_xml::Reader;
use quick_xml::events::Event;

use super::{
    AnswerInputs, DEVVM_DIR, LOG_FILES, NATIVE_DLL, NATIVE_SOURCE, TEMPLATES, answer_disk_files, answer_file,
    definition_hash, definition_hash_of, disk_files, probe_inputs, values,
};
use crate::guest::GuestOs;
use crate::guest::windows::WindowsGuest;
use crate::guest::windows::elevate::{self, caller_script};
use crate::guest::windows::pins::{ALL, Pin};
use crate::guest::windows::wrap::{BROKER_EXE, PWSH_EXE, literal};
use crate::media::SeedFile;

const PASSWORD: &str = "SENTINEL-Pw-1";

fn inputs() -> AnswerInputs<'static> {
    AnswerInputs {
        password: PASSWORD,
        image_index: 2,
        language: "de-DE",
        time_zone: "UTC",
        computer_name: "DEVVM",
    }
}

/// The raw text of a template, as the repository holds it.
fn raw(name: &str) -> &'static str {
    TEMPLATES
        .iter()
        .find(|(template, _)| *template == name)
        .unwrap_or_else(|| panic!("no template {name}"))
        .1
}

/// The text of a file of the answer disk.
fn rendered(name: &str) -> String {
    let files = answer_disk_files(&inputs()).unwrap();
    let file = files
        .iter()
        .find(|file| file.name == name)
        .unwrap_or_else(|| panic!("the answer disk has no {name}"));
    String::from_utf8(file.bytes.clone()).unwrap()
}

/// Where `needle` is in `text`; the test fails naming both when it is not.
fn at(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("`{needle}` is not in:\n{text}"))
}

fn assert_before(text: &str, first: &str, second: &str) {
    assert!(
        at(text, first) < at(text, second),
        "`{first}` must come before `{second}`"
    );
}

/// The lines of a batch file that run something: its `rem` lines say things, they do not do them.
fn commands(batch: &str) -> String {
    batch
        .lines()
        .filter(|line| !line.starts_with("rem "))
        .collect::<Vec<_>>()
        .join(
            "
",
        )
}

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn section<'a>(text: &'a str, from: &str, to: &str) -> &'a str {
    let start = at(text, from);
    let rest = &text[start..];
    &rest[..rest[from.len()..].find(to).map_or(rest.len(), |end| end + from.len())]
}

// The answer file =====================================================================================================

#[skuld::test]
fn the_answer_file_renders_deterministically_and_escapes_xml() {
    let hostile = AnswerInputs {
        password: "a<b>&\"c'd]]>",
        image_index: 3,
        language: "x<y",
        time_zone: "t&z",
        computer_name: "n\"m",
    };
    let text = answer_file(&hostile).unwrap();
    assert_eq!(
        text,
        answer_file(&hostile).unwrap(),
        "the same inputs render the same file"
    );
    assert_eq!(
        text.matches("a&lt;b&gt;&amp;&quot;c&apos;d]]&gt;").count(),
        2,
        "user and autologon"
    );
    assert!(!text.contains("a<b>"), "the password is escaped");
    assert!(text.contains("<UILanguage>x&lt;y</UILanguage>"));
    assert!(text.contains("<TimeZone>t&amp;z</TimeZone>"));
    assert!(text.contains("<ComputerName>n&quot;m</ComputerName>"));
    assert!(text.contains("<Key>/IMAGE/INDEX</Key>") && text.contains("<Value>3</Value>"));
    assert!(!text.contains("@@"), "every placeholder is filled");

    // The result is well-formed XML.
    let mut reader = Reader::from_str(&text);
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => panic!("the answer file is not XML: {e}\n{text}"),
        }
    }
}

#[skuld::test]
fn the_answer_file_names_no_disk_id_and_no_windowspe_driver_paths_and_offline_servicing_has_them() {
    let text = answer_file(&inputs()).unwrap();
    assert!(
        !text.contains("DiskID") && !text.contains("DiskConfiguration"),
        "Windows numbers the disks"
    );
    let windows_pe = section(&text, "pass=\"windowsPE\"", "<settings");
    assert!(
        !windows_pe.contains("DriverPaths") && !windows_pe.contains("PnpCustomizationsWinPE"),
        "with drvload they fail Setup"
    );
    let offline = section(&text, "pass=\"offlineServicing\"", "<settings");
    assert!(offline.contains("PnpCustomizationsNonWinPE"));
    assert!(offline.contains(r"viostor\w11\amd64") && offline.contains(r"NetKVM\w11\amd64"));
    let specialize = section(&text, "pass=\"specialize\"", "<settings");
    assert!(specialize.contains(r"NetKVM\w11\amd64"), "and the specialize pass");
    assert!(text.contains("<InstallToAvailablePartition>true</InstallToAvailablePartition>"));
}

#[skuld::test]
fn the_answer_file_has_no_recovery_partition_and_the_windows_partition_extends() {
    let text = answer_file(&inputs()).unwrap().to_lowercase();
    assert!(!text.contains("recovery"), "Setup would add one behind C:");
    let winpe = commands(&rendered("winpe.cmd"));
    let layout: Vec<&str> = winpe.lines().filter(|line| line.contains("create partition")).collect();
    assert_eq!(layout.len(), 3, "{layout:?}");
    assert!(layout[0].contains("efi") && layout[1].contains("msr"));
    assert!(
        layout[2].starts_with("echo create partition primary>>") && !layout[2].contains("size"),
        "the Windows partition takes the rest of the disk"
    );
    let lower = winpe.to_lowercase();
    assert!(
        !lower.contains("recovery") && !lower.contains("de94bba4"),
        "no recovery partition"
    );
}

#[skuld::test]
fn the_answer_file_logs_on_the_account_without_a_logon_count() {
    let text = answer_file(&inputs()).unwrap();
    assert!(!text.contains("LogonCount"));
    assert_eq!(text.matches("<Group>Administrators</Group>").count(), 1);
    assert!(text.contains("<Name>user</Name>"));
}

#[skuld::test]
fn the_specialize_marker_goes_to_the_com1_device_not_to_a_path_in_the_drive_root() {
    let text = answer_file(&inputs()).unwrap();
    assert!(text.contains(r"echo DEVVM-PROGRESS=specialize&gt;\\.\COM1"), "{text}");
    assert!(
        !text.contains(r"&gt;\.\COM1"),
        "the device is written with two leading backslashes: one names a file in the root of the current drive"
    );
}

#[skuld::test]
fn the_driver_paths_name_the_virtio_cd_on_every_letter_in_both_passes() {
    let text = answer_file(&inputs()).unwrap();
    for pass in ["offlineServicing", "specialize"] {
        let paths = section(&text, &format!("pass=\"{pass}\""), "<settings");
        for letter in 'C'..='Z' {
            for driver in ["viostor", "NetKVM"] {
                let path = format!("<Path>{letter}:\\{driver}\\w11\\amd64</Path>");
                assert!(paths.contains(&path), "{pass}: {path}");
            }
        }
        let keys: Vec<&str> = paths
            .split("wcm:keyValue=\"")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap())
            .collect();
        assert_eq!(keys.len(), 24 * 2, "{pass}");
        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), keys.len(), "{pass}: a key names one path");
    }
}

// winpe.cmd ===========================================================================================================

#[skuld::test]
fn winpe_cmd_finds_the_answer_disk_and_the_virtio_cd_by_file_not_by_letter_and_picks_the_system_disk_without_reading_diskpart_text()
 {
    let winpe = rendered("winpe.cmd");
    // The virtio CD is found by a file it holds, over every letter that can be a volume; WinPE can give the answer disk C:.
    assert!(winpe.contains(r"if exist %%d:\viostor\w11\amd64\viostor.inf set VIRTIO=%%d:"));
    assert!(winpe.contains("for %%d in (C D E F G H I J K"));
    // The answer disk is where the script is, and the answer file finds the script by itself.
    assert!(winpe.contains("%~dp0logs"));
    let answer = answer_file(&inputs()).unwrap();
    assert!(answer.contains(r"if exist %d:\winpe.cmd call %d:\winpe.cmd"));
    // The system disk is the first disk that does not hold the answer disk's volume label; no word or unit of diskpart's
    // is read, and no tool WinPE lacks.
    assert!(winpe.contains("detail disk") && winpe.contains("CIDATA"));
    let ran = commands(&winpe);
    assert!(!ran.contains("findstr") && !ran.contains(" GB") && !ran.contains("Disk 1"));
    // No drive letter is a literal: the volumes are found, never assumed.
    for forbidden in ["D:\\", "E:\\", "F:\\", "set DISK=1", "select disk 1"] {
        assert!(!ran.contains(forbidden), "{forbidden}");
    }
    // The partition is made before Setup looks, and the loop that waits for the disk ends only when it shows.
    assert_before(&winpe, "drvload", "create partition efi");
    assert!(!winpe.contains("TRIES% GTR") && !winpe.contains("TRIES% EQU"));
}

#[skuld::test]
fn winpe_cmd_is_written_with_crlf_and_names_the_logs_the_host_reads() {
    let winpe = rendered("winpe.cmd");
    assert!(
        !winpe.replace("\r\n", "").contains('\n'),
        "a bare line feed breaks cmd's labels"
    );
    assert!(winpe.contains("\r\n"));
    // Named files only: each log the host asks for is one the script copies, and the main log goes to the same folder.
    assert!(winpe.contains(r"logs\winpe.log"));
    for name in LOG_FILES {
        let name = name.rsplit('/').next().unwrap();
        assert!(winpe.contains(name), "{name}");
    }
}

// The scripts =========================================================================================================

#[skuld::test]
fn every_template_is_ascii_and_every_placeholder_is_filled() {
    for (name, text) in TEMPLATES {
        assert!(
            text.is_ascii(),
            "{name}: Windows PowerShell reads a file without a byte order mark as ANSI"
        );
        assert!(!text.contains('\r'), "{name}: templates are LF files");
        assert!(!text.is_empty());
    }
    for file in answer_disk_files(&inputs()).unwrap() {
        let text = String::from_utf8(file.bytes).unwrap();
        assert!(!text.contains("@@"), "{}", file.name);
    }
}

#[skuld::test]
fn the_answer_disk_holds_one_file_per_template() {
    let files = answer_disk_files(&inputs()).unwrap();
    let mut names: Vec<&str> = files.iter().map(|file| file.name.as_str()).collect();
    names.sort_unstable();
    let mut wanted: Vec<&str> = TEMPLATES.iter().map(|(name, _)| *name).collect();
    wanted.sort_unstable();
    assert_eq!(names, wanted);
    assert_eq!(
        wanted,
        [
            "Autounattend.xml",
            "Devvm.cs",
            "boot-task.ps1",
            "bootstrap.ps1",
            "broker-config.yaml",
            "eb-runner.ps1",
            "lib.ps1",
            "sshd_config",
            "winpe.cmd"
        ]
    );
}

#[skuld::test]
fn no_template_embeds_a_secret_but_the_per_build_password() {
    let files = answer_disk_files(&inputs()).unwrap();
    for file in &files {
        let text = String::from_utf8(file.bytes.clone()).unwrap();
        let count = text.matches(PASSWORD).count();
        if file.name == "Autounattend.xml" {
            assert_eq!(count, 2, "the account and the autologon");
        } else {
            assert_eq!(count, 0, "{}", file.name);
        }
    }
    for (name, text) in TEMPLATES {
        assert!(!text.contains("PRIVATE KEY"), "{name}");
        assert_eq!(
            text.matches("@@PASSWORD@@").count(),
            if *name == "Autounattend.xml" { 2 } else { 0 },
            "{name}"
        );
        assert_eq!(text.contains("PlainText"), *name == "Autounattend.xml", "{name}");
    }
}

#[skuld::test]
fn the_bootstrap_sets_password_never_expires_and_no_logon_count() {
    let bootstrap = rendered("bootstrap.ps1");
    assert!(bootstrap.contains("Set-LocalUser -Name 'user' -PasswordNeverExpires $true"));
    assert!(bootstrap.contains("net.exe accounts /maxpwage:unlimited"));
    // `PasswordNeverExpires` is no property of LocalUser: the check reads the CIM class's `PasswordExpires`.
    assert!(bootstrap.contains("Win32_UserAccount") && bootstrap.contains(".PasswordExpires -ne $false"));
    assert!(!bootstrap.contains("(Get-LocalUser -Name 'user').PasswordNeverExpires"));
    assert!(bootstrap.contains("Remove-ItemProperty -Path $winlogon -Name AutoLogonCount"));
    assert!(!bootstrap.contains("Set-ItemProperty -Path $winlogon -Name AutoLogonCount"));
}

#[skuld::test]
fn the_bootstrap_resets_the_openssh_acl_after_install() {
    let bootstrap = rendered("bootstrap.ps1");
    assert_before(&bootstrap, "install-sshd.ps1", "icacls.exe $sshd /reset /T");
    assert_before(&bootstrap, "Move-Item", "install-sshd.ps1");
}

#[skuld::test]
fn the_bootstrap_catches_and_writes_the_failed_marker_before_shutdown() {
    let bootstrap = rendered("bootstrap.ps1");
    assert_before(&bootstrap, "} catch {", "DEVVM-BASE-FAILED=");
    assert_before(&bootstrap, "DEVVM-BASE-FAILED=", "shutdown.exe /s");
    assert_before(&bootstrap, "Send-Line 'DEVVM-BASE-DONE'", "shutdown.exe /s");
    assert_eq!(
        bootstrap.matches("shutdown.exe").count(),
        1,
        "one shutdown, whatever happened"
    );
    assert!(
        bootstrap.contains("if (-not $failed) { Send-Line 'DEVVM-BASE-DONE' }"),
        "done only without a failure"
    );
    assert_before(&bootstrap, "Send-Line \"DEVVM-LICENSE=", "Send-Line 'DEVVM-BASE-DONE'");
}

#[skuld::test]
fn a_read_with_no_licensed_product_is_sent_as_null_not_thrown() {
    let bootstrap = rendered("bootstrap.ps1");
    let read = section(&bootstrap, "$license = Get-CimInstance", "Send-Line \"DEVVM-LICENSE=");
    assert!(
        !read.contains("throw") && !bootstrap.contains("no licensed Windows product"),
        "a retail install with no key has no row, and the host decides what that means"
    );
    assert!(read.contains("'null'"), "{read}");
}

#[skuld::test]
fn the_native_helper_is_compiled_once_by_the_bootstrap_and_only_loaded_by_the_scripts_that_use_it() {
    assert!(TEMPLATES.iter().any(|(name, _)| *name == NATIVE_SOURCE));
    let bootstrap = rendered("bootstrap.ps1");
    assert!(bootstrap.contains(&format!("(Join-Path $PSScriptRoot {})", literal(NATIVE_SOURCE))));
    assert!(bootstrap.contains("Add-Type -TypeDefinition") && bootstrap.contains("-OutputAssembly"));
    assert!(bootstrap.contains(&format!("(Join-Path $scripts {})", literal(NATIVE_DLL))));
    assert_before(&bootstrap, "-OutputAssembly", "Register-ScheduledTask");
    for name in ["eb-runner.ps1", "boot-task.ps1"] {
        let script = rendered(name);
        assert!(
            !script.contains("Add-Type -TypeDefinition"),
            "{name} compiles at every start"
        );
        assert!(
            script.contains(&format!(
                "Add-Type -LiteralPath (Join-Path $PSScriptRoot {})",
                literal(NATIVE_DLL)
            )),
            "{name}"
        );
    }
}

#[skuld::test]
fn every_download_is_checked_against_its_pin_before_it_is_used() {
    let bootstrap = rendered("bootstrap.ps1");
    for pin in ALL.iter().filter(|pin| pin.url.ends_with(".zip")) {
        assert!(
            bootstrap.contains(&format!("Get-Pinned {} {}", literal(pin.url), literal(pin.sha256))),
            "{}",
            pin.what
        );
    }
    assert_before(
        &bootstrap,
        "Get-FileHash",
        "throw \"sha256 of $Url is $actual, expected $Sha256\"",
    );
    assert!(
        bootstrap.contains("expected $Sha256"),
        "the failure names the URL, the expected and the actual"
    );
}

#[skuld::test]
fn user_scripts_use_driveinfo_never_get_volume_and_the_serial_mutex_has_an_everyone_acl() {
    for (name, text) in TEMPLATES {
        assert!(!text.contains("Get-Volume"), "{name}");
    }
    let lib = rendered("lib.ps1");
    assert!(lib.contains("[System.IO.DriveInfo]::GetDrives()"));
    // The mutex is made by whichever process is first, SYSTEM or `user`, under one name.
    assert!(lib.contains("S-1-1-0") && lib.contains("MutexAccessRule") && lib.contains("'Global\\devvm-serial'"));
    assert!(lib.contains("MutexAcl]::Create") && lib.contains("Mutex]::new($false, $name, [ref]$created, $security)"));
    assert!(lib.contains("COM1") && lib.contains("UTF8"));
    // The first-logon script finds the answer disk by DriveInfo too.
    assert!(answer_file(&inputs()).unwrap().contains("[IO.DriveInfo]::GetDrives()"));
}

#[skuld::test]
fn sshd_config_names_only_the_seeded_ed25519_host_key_and_sshd_is_manual() {
    let config = rendered("sshd_config");
    let host_keys: Vec<&str> = config
        .lines()
        .filter(|line| line.trim_start().starts_with("HostKey"))
        .collect();
    assert_eq!(host_keys, ["HostKey __PROGRAMDATA__/ssh/ssh_host_ed25519_key"]);
    for other in ["rsa", "ecdsa", "dsa", "mldsa"] {
        assert!(!config.contains(&format!("ssh_host_{other}")), "{other}");
    }
    assert!(config.contains("PasswordAuthentication no") && config.contains("PubkeyAuthentication yes"));
    assert!(config.contains("administrators_authorized_keys"));
    let bootstrap = rendered("bootstrap.ps1");
    assert!(bootstrap.contains("Set-Service -Name sshd -StartupType Manual"));
    assert!(
        !bootstrap.contains("Start-Service"),
        "the boot task starts sshd, never the base build"
    );
    // The base holds no host key, and the config is protected like the keys.
    assert!(bootstrap.contains("Get-ChildItem -LiteralPath $ssh -Filter 'ssh_host_*' -Force | Remove-Item -Force"));
    assert_before(&bootstrap, "Remove-Item -Force", "sshd_config");
    assert!(bootstrap.contains("Protect-File \"$ssh\\sshd_config\""));
}

#[skuld::test]
fn the_bootstrap_puts_the_pwsh_directory_on_the_machine_path() {
    let bootstrap = rendered("bootstrap.ps1");
    assert!(bootstrap.contains("$pwshDir = Split-Path -Parent $pwsh"));
    assert!(bootstrap.contains(&format!("$pwsh = {}", literal(PWSH_EXE))));
    assert!(bootstrap.contains("GetEnvironmentVariable('Path', 'Machine')"));
    assert!(bootstrap.contains("SetEnvironmentVariable('Path', \"$machinePath;$pwshDir\", 'Machine')"));
    // Before the broker starts the agent that the commands run under.
    assert_before(&bootstrap, "SetEnvironmentVariable", "ssh-broker.exe\" apply");
}

#[skuld::test]
fn the_templates_and_wrap_share_the_pwsh_and_broker_path_constants() {
    let bootstrap = rendered("bootstrap.ps1");
    assert!(bootstrap.contains(&format!("$brokerExe = {}", literal(BROKER_EXE))));
    assert!(rendered("broker-config.yaml").contains(&format!("start: ['{PWSH_EXE}']")));
    assert!(rendered("eb-runner.ps1").contains(&format!("$pwsh = {}", literal(PWSH_EXE))));
    for (name, text) in TEMPLATES {
        assert!(
            !text.contains("PowerShell\\7") && !text.contains("ProgramData\\ssh-broker"),
            "{name} copies a path"
        );
    }
}

// The boot task =======================================================================================================

#[skuld::test]
fn the_boot_task_starts_sshd_last_after_the_agent() {
    let task = rendered("boot-task.ps1");
    assert_before(&task, "DEVVM-SEED-WRITTEN", "Write-Whole $bootIdFile");
    assert_before(&task, "Write-Whole $bootIdFile", "Resize-Partition");
    assert_before(&task, "Resize-Partition", "Test-AgentSocket $sid");
    assert_before(&task, "Test-AgentSocket $sid", "Start-Service -Name sshd");
    assert_eq!(task.matches("Start-Service").count(), 1);
    let after = &task[at(&task, "Start-Service -Name sshd")..];
    assert!(after.trim_end().ends_with('}') && !after.contains("Resize") && !after.contains("Send-Line \"DEVVM-SEED"));
}

#[skuld::test]
fn the_boot_task_does_not_read_a_write_back_in_the_same_boot() {
    // The page cache would answer with the bytes just written, so a torn write could not show; the next boot rewrites the
    // seed anyway.
    let task = rendered("boot-task.ps1");
    assert!(!task.contains("ReadAllBytes($target)"));
    assert!(!task.contains("-cne") && !task.contains("differs from the seed"));
}

#[skuld::test]
fn the_boot_task_writes_a_seed_marker_after_each_rename_and_a_fresh_boot_id_before_sshd() {
    let task = rendered("boot-task.ps1");
    // A temp file, flushed to the disk, renamed over the target, and the directory flushed; the marker follows each
    // file's write.
    assert_before(&task, "Flush($true)", "[IO.File]::Move($temp, $Target, $true)");
    assert_before(
        &task,
        "[IO.File]::Move($temp, $Target, $true)",
        "[Devvm.Disk]::FlushDirectory(",
    );
    assert_before(&task, "Write-Whole $target", "DEVVM-SEED-WRITTEN=$name");
    assert!(task.contains("$seed = [ordered]@{") && task.contains("administrators_authorized_keys"));
    // The boot id is random and fresh at every boot, in the file `boot_id_command` reads, readable by `Users`.
    assert!(task.contains("RandomNumberGenerator]::Fill($bytes)"));
    assert!(task.contains(&format!("$bootIdFile = {}", literal(super::super::BOOT_ID_FILE))));
    assert!(task.contains("@('S-1-5-32-545')"));
    assert_before(&task, "Write-Whole $bootIdFile", "Start-Service");
}

#[skuld::test]
fn the_boot_task_creates_each_file_with_its_acl_before_any_byte_is_written() {
    let task = rendered("boot-task.ps1");
    let write = section(
        &task,
        "function Write-Whole",
        "
}
",
    );
    // The descriptor is given to the create call: no moment exists in which the file is readable by anyone else.
    assert!(write.contains("[IO.FileSystemAclExtensions]::Create("), "{write}");
    assert_before(
        write,
        "SetAccessRuleProtection($true, $false)",
        "[IO.FileSystemAclExtensions]::Create(",
    );
    assert_before(write, "[IO.FileSystemAclExtensions]::Create(", "$stream.Write(");
    assert_before(
        write,
        "[IO.File]::Delete($temp)",
        "[IO.FileSystemAclExtensions]::Create(",
    );
    assert!(
        write.contains("[IO.FileMode]::CreateNew"),
        "a leftover file is never reused with its old ACL"
    );
    for sid in ["'S-1-5-18'", "'S-1-5-32-544'"] {
        assert!(write.contains(sid), "{sid}");
    }
    // No write opens the file without a descriptor, and no icacls runs after the bytes are on the disk.
    assert!(!task.contains("icacls") && !write.contains("[IO.File]::Open("));
}

#[skuld::test]
fn the_boot_task_fails_closed_on_a_failed_boot_id_write_or_resize() {
    let task = rendered("boot-task.ps1");
    let body = &task[at(
        &task, "
try {",
    )..at(&task, "} catch {")];
    for step in ["Write-Whole $bootIdFile", "Resize-Partition", "Start-Service"] {
        assert!(body.contains(step), "{step} is inside the try");
    }
    let handler = &task[at(&task, "} catch {")..];
    assert!(handler.contains("DEVVM-BOOT-FAILED="));
    assert!(!handler.contains("Start-Service"), "a failed boot never starts sshd");
    assert!(handler.contains("exit 1"));
    assert!(task.contains("$ErrorActionPreference = 'Stop'"));
}

#[skuld::test]
fn the_boot_task_waits_for_a_connect_to_the_agent_socket_not_for_a_process() {
    let task = rendered("boot-task.ps1");
    assert!(task.contains("UnixDomainSocketEndPoint") && task.contains(".Connect("));
    assert!(task.contains(r"AppData\Local\ssh-broker\agent.sock"));
    assert!(task.contains("ProfileList\\$Sid"), "the profile is found by SID");
    assert!(!task.contains("Get-Process") && !task.contains("Wait-Process") && !task.contains("Get-ScheduledTask"));
    // A condition re-checked until it holds: no count, no time bound.
    assert!(task.contains("while (-not (Test-AgentSocket $sid)) { Start-Sleep -Milliseconds 200 }"));
    for bound in ["-Timeout", "$tries", "$attempt", "Stopwatch"] {
        assert!(!task.contains(bound), "{bound}");
    }
}

#[skuld::test]
fn the_boot_task_sweeps_the_request_directories_nothing_else_removed() {
    let task = rendered("boot-task.ps1");
    assert_eq!(task.matches("Remove-Item").count(), 1);
    assert!(task.contains("Get-ChildItem -LiteralPath $root -Force | Remove-Item -Recurse -Force"));
    assert!(task.contains(&format!("$root = {}", literal(elevate::ROOT))));
    assert_before(&task, "Remove-Item", "DEVVM-SEED-WRITTEN");
}

// The elevated helper =================================================================================================

#[skuld::test]
fn the_eb_runner_serves_only_the_request_named_by_its_first_argument_and_claims_it_with_a_held_create_new() {
    let runner = rendered("eb-runner.ps1");
    assert!(runner.contains("param([string]$Id)") && runner.contains("'^[0-9a-f]{32}$'"));
    assert!(runner.contains("$dir = Join-Path $root $Id"));
    assert!(runner.contains("[IO.File]::Open((Join-Path $dir $claimName), 'CreateNew', 'Write', 'None')"));
    assert!(
        runner.contains("if ($null -eq $claim) { exit 0 }"),
        "a request with a claim, or none, is not served"
    );
    // The claim is held until the end, and the request goes after it is released.
    assert_before(&runner, "'CreateNew', 'Write', 'None')", "$claim.Dispose()");
    // The only walk over the other requests is the sweep, which never serves one.
    assert_eq!(runner.matches("GetDirectories(").count(), 1);
    assert_before(&runner, "foreach ($other in", "$dir = Join-Path $root $Id");
    let sweep = section(&runner, "foreach ($other in", "$dir = Join-Path");
    assert!(
        sweep.contains("$name -eq $Id") && sweep.contains("EndsWith('.tmp')") && !sweep.contains("Process]::Start")
    );
}

#[skuld::test]
fn the_eb_runner_removes_requests_whose_caller_ended_and_holds_no_claim() {
    let runner = rendered("eb-runner.ps1");
    // A request goes when its caller is gone (or its id now belongs to a process that started at another time) and no
    // runner holds its claim; the helper answers both from one handle.
    assert!(runner.contains("[Devvm.Caller]::Open("));
    assert!(runner.contains("(Test-Held (Join-Path $other $claimName))"));
    assert!(runner.contains("[IO.File]::Open($Path, 'Open', 'ReadWrite', 'None')"));
    // No time bound anywhere.
    for bound in ["-Timeout", "WaitForExit(", "Stopwatch", "Thread.Sleep"] {
        assert!(!runner.contains(bound), "{bound}");
        assert!(!raw(NATIVE_SOURCE).contains(bound), "{bound}");
    }
}

#[skuld::test]
fn the_runner_checks_its_caller_and_waits_for_it_on_one_handle_it_opened_once() {
    let runner = rendered("eb-runner.ps1");
    let native = raw(NATIVE_SOURCE);
    // PowerShell looks no process up: a second lookup could name a process that reused the id.
    for lookup in ["GetProcessById", "StartTime", "Get-Process"] {
        assert!(!runner.contains(lookup), "{lookup}");
        assert!(!native.contains(lookup), "{lookup}");
    }
    let caller = section(native, "public static Caller Open(", "public Task Exited");
    assert_eq!(caller.matches("OpenProcess(").count(), 1, "{caller}");
    assert_before(caller, "OpenProcess(", "GetProcessTimes(");
    assert_before(caller, "GetProcessTimes(", "new Watch(handle)");
    assert!(
        caller.contains("startTicks")
            && caller.contains("DateTime.FromFileTime(")
            && caller.contains(".ToUniversalTime().Ticks"),
        "the start time is compared in the ticks the caller wrote"
    );
    // The main path asks once and uses what it was given.
    let main = section(&runner, "# One handle on the caller", "$finished.Wait()");
    assert!(main.contains("$caller.Exited"), "{main}");
}

#[skuld::test]
fn the_runner_starts_its_child_in_a_job_of_its_own_and_ends_the_tree_by_the_job_never_by_an_id() {
    let runner = rendered("eb-runner.ps1");
    let native = raw(NATIVE_SOURCE);
    // The child is born suspended, put in a kill-on-close job, and only then runs: no process of its tree is outside it.
    assert!(
        native.contains("const uint CreateSuspended = 0x4;") && native.contains("const int KillOnJobClose = 0x2000;")
    );
    assert!(native.contains("CreateSuspended |"));
    assert_before(
        native,
        "if (!Native.CreateProcessW(",
        "if (!Native.AssignProcessToJobObject(job, process))",
    );
    assert_before(
        native,
        "if (!Native.AssignProcessToJobObject(job, process))",
        "Native.ResumeThread(thread)",
    );
    // The tree ends with the job; no process is looked up by id to be ended, and the runner is not in the job.
    assert!(native.contains("Native.TerminateJobObject(job"));
    for by_id in [
        "Kill(",
        "KillOthers",
        "EnterOwnJob",
        "GetProcessById",
        "BasicProcessIdList",
        "TerminateProcess(",
    ] {
        assert!(!native.contains(by_id), "{by_id}");
        assert!(!runner.contains(by_id), "{by_id}");
    }
    assert!(runner.contains("[Devvm.Child]::Start(") && runner.contains(".Terminate()"));
    assert!(!runner.contains("Process]::Start") && !runner.contains("Add-Type -TypeDefinition"));
    // Whatever the runner leaves behind goes with the job when the runner ends, however it ends.
    assert!(runner.contains("$child.Dispose()"));
}

#[skuld::test]
fn the_eb_runner_writes_stdout_stderr_exit_and_done_in_that_order_and_removes_its_request_after_read() {
    let runner = rendered("eb-runner.ps1");
    // Output as the command makes it, unbuffered, shared for reading; then the status; `done` last.
    assert!(runner.contains("[IO.FileStream]::new((Join-Path $dir $stdoutName), 'Create', 'Write', 'Read', 1)"));
    assert!(runner.contains("[IO.FileStream]::new((Join-Path $dir $stderrName), 'Create', 'Write', 'Read', 1)"));
    assert_before(&runner, "CopyToAsync($out)", "$exitName), $exitCode");
    assert_before(
        &runner,
        "$exitName), $exitCode",
        "WriteAllText((Join-Path $dir $doneName)",
    );
    assert!(
        runner.contains("$exitCode.ToString([Globalization.CultureInfo]::InvariantCulture)"),
        "signed decimal"
    );
    // The child: PowerShell 7 with the file, input at end of file (the helper closes the write end of its input pipe).
    assert!(runner.contains("-NoProfile -NonInteractive -File"));
    assert!(raw(NATIVE_SOURCE).contains("Close(ref inWrite)"));
    assert!(runner.contains("$child.Output.CopyToAsync($out)") && runner.contains("$child.Error.CopyToAsync($err)"));
    // Then the wait for `read`, or for the caller's end, and the request goes last.
    assert_before(
        &runner,
        "WriteAllText((Join-Path $dir $doneName)",
        "Exists((Join-Path $dir $readName))",
    );
    assert_before(&runner, "Exists((Join-Path $dir $readName))", "$claim.Dispose()");
    assert_before(
        &runner,
        "$claim.Dispose()",
        "Remove-Item -LiteralPath $dir -Recurse -Force",
    );
    assert!(
        runner
            .trim_end()
            .ends_with("try { Remove-Item -LiteralPath $dir -Recurse -Force } catch { }")
    );
    // The runner sets UTF-8 output, like every PowerShell devvm starts.
    assert!(runner.contains("[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)"));
}

#[skuld::test]
fn the_eb_task_is_registered_for_user_with_an_interactive_token_the_highest_run_level_parallel_instances_and_the_request_id_as_its_argument()
 {
    let bootstrap = rendered("bootstrap.ps1");
    let registration = section(&bootstrap, "$helperSettings", "Remove-Item -LiteralPath $work");
    assert!(registration.contains("-MultipleInstances Parallel"));
    assert!(
        registration.contains("eb-runner.ps1\" $(Arg0)'"),
        "the request id is the task's first argument"
    );
    assert!(registration.contains("-UserId 'user' -LogonType Interactive -RunLevel Highest"));
    assert!(registration.contains(&format!("Register-ScheduledTask -TaskName {}", literal(elevate::TASK))));
    assert!(
        !bootstrap.contains("S4U"),
        "a logon type that gives no interactive session"
    );
    // The boot task is SYSTEM's, at startup, with no time limit: it waits for the agent as long as it takes.
    assert!(bootstrap.contains("New-ScheduledTaskTrigger -AtStartup"));
    assert!(bootstrap.contains("-UserId 'S-1-5-18' -LogonType ServiceAccount -RunLevel Highest"));
    assert_eq!(bootstrap.matches("-ExecutionTimeLimit ([TimeSpan]::Zero)").count(), 2);
    // Both run from a directory only administrators write.
    assert!(bootstrap.contains("$scripts = 'C:\\Program Files\\devvm'"));
}

#[skuld::test]
fn the_runner_and_the_callers_names_come_from_the_same_constants() {
    let runner = rendered("eb-runner.ps1");
    let task = rendered("boot-task.ps1");
    let bootstrap = rendered("bootstrap.ps1");
    let caller = caller_script("inner");
    for (variable, value) in [
        ("$root", elevate::ROOT),
        ("$scriptName", elevate::SCRIPT),
        ("$callerName", elevate::CALLER),
        ("$claimName", elevate::CLAIM),
        ("$stdoutName", elevate::STDOUT),
        ("$stderrName", elevate::STDERR),
        ("$exitName", elevate::EXIT),
        ("$doneName", elevate::DONE),
        ("$readName", elevate::READ),
    ] {
        assert!(
            runner.contains(&format!("{variable} = {}", literal(value))),
            "{variable}"
        );
        assert!(caller.contains(value), "the caller names {value}");
    }
    assert!(task.contains(&format!("$root = {}", literal(elevate::ROOT))));
    assert!(bootstrap.contains(&format!("-TaskName {}", literal(elevate::TASK))));
    assert!(caller.contains(&literal(elevate::TASK)));
    // The request directory is under the data directory; so is the boot id.
    assert!(elevate::ROOT.starts_with(&format!("{DEVVM_DIR}\\")));
    assert!(super::super::BOOT_ID_FILE.starts_with(&format!("{DEVVM_DIR}\\")));
    // Nothing in a raw template is a copy of them.
    for (name, text) in TEMPLATES {
        assert!(!text.contains("ProgramData\\devvm"), "{name}");
        assert!(!text.contains("devvm-eb"), "{name}");
    }
}

// The definition hash =================================================================================================

fn file(name: &str, text: &str) -> SeedFile {
    SeedFile {
        name: name.to_owned(),
        bytes: text.as_bytes().to_vec(),
    }
}

fn copy(files: &[SeedFile]) -> Vec<SeedFile> {
    files
        .iter()
        .map(|file| SeedFile {
            name: file.name.clone(),
            bytes: file.bytes.clone(),
        })
        .collect()
}

#[skuld::test]
fn the_definition_hash_changes_with_each_file_and_each_pin() {
    let version = WindowsGuest.semantics_version();
    let files = answer_disk_files(&probe_inputs()).unwrap();
    let base = definition_hash_of(&files, ALL, version);
    assert_eq!(base, definition_hash_of(&files, ALL, version), "a pure function");
    assert_eq!(
        base,
        definition_hash(),
        "the builder's hash is that of the rendered templates and the pins"
    );

    for at in 0..files.len() {
        let mut changed = copy(&files);
        changed[at].bytes.push(b'x');
        assert_ne!(
            base,
            definition_hash_of(&changed, ALL, version),
            "the text of {}",
            files[at].name
        );
        let mut changed = copy(&files);
        changed[at].name = "renamed".to_owned();
        assert_ne!(
            base,
            definition_hash_of(&changed, ALL, version),
            "the name of {}",
            files[at].name
        );
    }
    for at in 0..ALL.len() {
        let other_hash = leak(format!("{}0", &ALL[at].sha256[..63]));
        let other_url = leak(format!("{}x", ALL[at].url));
        for changed in [
            Pin {
                what: ALL[at].what,
                url: ALL[at].url,
                sha256: other_hash,
            },
            Pin {
                what: ALL[at].what,
                url: other_url,
                sha256: ALL[at].sha256,
            },
            Pin {
                what: "another",
                url: ALL[at].url,
                sha256: ALL[at].sha256,
            },
        ] {
            let mut pins: Vec<&Pin> = ALL.to_vec();
            pins[at] = &changed;
            assert_ne!(
                base,
                definition_hash_of(&files, &pins, version),
                "the pin {}",
                ALL[at].what
            );
        }
    }
    assert_ne!(
        base,
        definition_hash_of(&files, ALL, version + 1),
        "the semantics version"
    );
    assert_ne!(base, definition_hash_of(&files[1..], ALL, version), "a file taken away");
}

#[skuld::test]
fn the_definition_hash_changes_with_every_value_the_templates_substitute_but_the_password() {
    let version = WindowsGuest.semantics_version();
    let probe = probe_inputs();
    let base = definition_hash();
    for (name, value) in values(&probe) {
        if name == "PASSWORD" {
            continue;
        }
        let mut changed = values(&probe);
        changed.iter_mut().find(|(known, _)| *known == name).unwrap().1 = format!("{value}x");
        let files = disk_files(TEMPLATES, &changed).unwrap();
        assert_ne!(base, definition_hash_of(&files, ALL, version), "{name}");
    }
}

#[skuld::test]
fn the_definition_hash_does_not_collide_when_bytes_move_between_a_name_and_its_text() {
    let one = [file("ab", "c")];
    let other = [file("a", "bc")];
    assert_ne!(definition_hash_of(&one, &[], 1), definition_hash_of(&other, &[], 1));
}
