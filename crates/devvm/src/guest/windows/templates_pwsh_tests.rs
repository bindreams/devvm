//! The answer disk's scripts and helper in real PowerShells (the `pwsh_wrap` group, which the CI Windows Test lane and
//! the deputy's runner run; a developer's PC may not). The text tests of `templates_tests.rs` cannot see a syntax error
//! or a compile error; these can. What the scripts do on a guest is measured on a guest.

use std::process::Output;

use quick_xml::Reader;
use quick_xml::events::Event;

use super::{NATIVE_DLL, NATIVE_SOURCE, TEMPLATES, answer_disk_files, answer_file, disk_files};
use crate::guest::windows::templates::templates_tests::inputs;
use crate::guest::windows::wrap::literal;
use crate::guest::windows::wrap::wrap_pwsh_tests::{HOSTILE, Search, run, run_with, shown};
use crate::test_groups::{Group, pwsh_wrap};

/// `script` in Windows PowerShell 5.1 (`powershell.exe`), the PowerShell the guest's first-logon command and bootstrap
/// run in.
fn run_windows_powershell(script: &str) -> Output {
    run_with("powershell.exe", script, None, Search::Caller)
}

/// The PowerShells that run each script on the guest: the first-logon command runs `bootstrap.ps1` (5.1); the tasks it
/// registers run the others (7); `lib.ps1` is dot-sourced by both.
const INTERPRETERS: [(&str, &[&str]); 4] = [
    ("bootstrap.ps1", &["powershell.exe"]),
    ("lib.ps1", &["powershell.exe", "pwsh"]),
    ("boot-task.ps1", &["pwsh"]),
    ("eb-runner.ps1", &["pwsh"]),
];

/// A script that parses `input` with the parser method `method` (`ParseFile` takes a path, `ParseInput` the text) and
/// prints its parse errors, one per line, with their line numbers. It never runs `input`.
fn parse_errors_script(method: &str, input: &str) -> String {
    r#"$ErrorActionPreference = 'Stop'
$errors = $null; $tokens = $null
[void][System.Management.Automation.Language.Parser]::@@METHOD@@(@@INPUT@@, [ref]$tokens, [ref]$errors)
foreach ($e in $errors) { "$($e.Extent.StartLineNumber): $($e.Message)" }
"#
    .replace("@@METHOD@@", method)
    // Last, so the input is never scanned for a placeholder.
    .replace("@@INPUT@@", &literal(input))
}

/// The parse errors of the script `text` in the grammar of the PowerShell `exe`, read from a file as that PowerShell
/// reads a script it runs. Windows PowerShell 5.1 reads it in this host's ANSI code page, not the guest's, so `text` must
/// be ASCII.
fn parse_errors(exe: &str, dir: &std::path::Path, name: &str, text: &[u8]) -> Output {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    run_with(
        exe,
        &parse_errors_script("ParseFile", &path.display().to_string()),
        None,
        Search::Caller,
    )
}

#[skuld::test]
fn every_script_of_the_answer_disk_parses_in_each_powershell_that_runs_it(#[fixture(pwsh_wrap)] _group: &Group) {
    let dir = tempfile::tempdir().unwrap();
    let mut scripts: Vec<_> = answer_disk_files(&inputs())
        .unwrap()
        .into_iter()
        .filter(|file| file.name.ends_with(".ps1"))
        .collect();
    scripts.sort_by(|a, b| a.name.cmp(&b.name));
    let mut named: Vec<_> = INTERPRETERS.iter().map(|(name, _)| *name).collect();
    named.sort();
    assert_eq!(
        scripts.iter().map(|file| file.name.as_str()).collect::<Vec<_>>(),
        named,
        "every script of the answer disk names the PowerShells that run it"
    );
    for (name, exes) in INTERPRETERS {
        let file = scripts.iter().find(|file| file.name == name).unwrap();
        assert!(file.bytes.is_ascii(), "{name} as rendered is not ASCII");
        for exe in exes {
            let output = parse_errors(exe, dir.path(), name, &file.bytes);
            assert_eq!(output.status.code(), Some(0), "{name} in {exe}: {}", shown(&output));
            let errors = String::from_utf8_lossy(&output.stdout);
            assert!(errors.trim().is_empty(), "{name} does not parse in {exe}:\n{errors}");
        }
    }
}

#[skuld::test]
fn the_native_helper_compiles_as_the_bootstrap_compiles_it_and_the_scripts_can_load_it(
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    // A plain directory: the C# 5 compiler Windows PowerShell 5.1 uses gets its paths in the ANSI code page.
    let dir = tempfile::tempdir().unwrap();
    let dll = dir.path().join(NATIVE_DLL);

    // Windows PowerShell 5.1, as the first-logon script does: C#, written to an assembly on disk.
    compile_native_helper(dir.path());
    assert!(dll.is_file(), "no assembly was written");

    // PowerShell 7 loads it as the runner and the boot task do, and finds the three types they use.
    let load = format!(
        "$ErrorActionPreference = 'Stop'; Add-Type -LiteralPath {}; foreach ($name in 'Devvm.Caller', 'Devvm.Child', 'Devvm.Disk') {{ ([type]$name).FullName }}",
        literal(&dll.display().to_string())
    );
    let loaded = run(&load, "none", None);
    assert_eq!(loaded.status.code(), Some(0), "{}", shown(&loaded));
    let names: Vec<String> = String::from_utf8_lossy(&loaded.stdout)
        .lines()
        .map(|line| line.trim().to_owned())
        .collect();
    assert_eq!(
        names,
        ["Devvm.Caller", "Devvm.Child", "Devvm.Disk"],
        "{}",
        shown(&loaded)
    );
}

#[skuld::test]
fn a_hostile_value_in_a_powershell_template_reads_back_as_itself(#[fixture(pwsh_wrap)] _group: &Group) {
    let template = [(
        "t.ps1",
        "$v = @@V@@\n[Convert]::ToHexString([Text.Encoding]::UTF8.GetBytes($v)).ToLowerInvariant()\n",
    )];
    assert!(TEMPLATES.iter().all(|(name, _)| *name != "t.ps1"));
    let extra = ["@@", "@@V@@", "it's @@V@@", "'; Write-Output pwned; '"];
    for value in HOSTILE.iter().chain(&extra) {
        let files = disk_files(&template, &[("V", (*value).to_owned())]).unwrap();
        let script = String::from_utf8(files[0].bytes.clone()).unwrap();
        let output = run(&script, "none", None);
        assert_eq!(output.status.code(), Some(0), "{value:?}: {}", shown(&output));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            hex::encode(value.as_bytes()),
            "{value:?}: {}",
            shown(&output)
        );
    }
}

// The first-logon command =============================================================================================

/// The script of the first-logon command of `answer`, as the XML parser reads it (entities undone) with the
/// `powershell.exe ... -Command "` frame taken off. The frame is not argv-unquoted: the text is what `powershell.exe`
/// receives only if it holds no `"` (a `\"` has one too) and does not end in a `\` (it would escape the closing `"`), so
/// a script with either is refused.
fn first_logon_script(answer: &str) -> String {
    let mut reader = Reader::from_str(answer);
    loop {
        match reader.read_event().expect("the answer file is XML") {
            Event::Start(start) if start.name().as_ref() == "CommandLine" => {
                let raw = reader.read_text(start.name()).unwrap();
                let line = quick_xml::escape::unescape(&raw.xml10_content()).unwrap().into_owned();
                let script = line
                    .strip_prefix("powershell.exe -NoProfile -ExecutionPolicy Bypass -Command \"")
                    .and_then(|rest| rest.strip_suffix('"'))
                    .unwrap_or_else(|| panic!("not a powershell.exe -Command line: {line}"));
                assert!(
                    !script.contains('"'),
                    "a `\"` inside the -Command frame needs argv quoting: {line}"
                );
                assert!(
                    !script.ends_with('\\'),
                    "a `\\` before the closing `\"` escapes it: {line}"
                );
                return script.to_owned();
            }
            Event::Eof => panic!(
                "the answer file has no first-logon command:
{answer}"
            ),
            _ => {}
        }
    }
}

/// What the command does is pinned by `the_first_logon_command_waits_for_the_answer_disk_with_no_cap`; this is that it
/// parses in Windows PowerShell 5.1, which runs it on the guest.
#[skuld::test]
fn the_first_logon_command_parses_in_windows_powershell(#[fixture(pwsh_wrap)] _group: &Group) {
    let script = first_logon_script(&answer_file(&inputs()).unwrap());
    let output = run_windows_powershell(&parse_errors_script("ParseInput", &script));
    assert_eq!(output.status.code(), Some(0), "{}", shown(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "", "{script}");
}

// The boot task's decisions ===========================================================================================

/// What the boot task's rename decision starts from: the computer's name, the seed's `meta.json` (absent: no disk file),
/// the name the last boot restarted for, and the statuses `shutdown.exe` answers with, one for each request.
struct Rename<'a> {
    computer: &'a str,
    meta: Option<&'a str>,
    marker: Option<&'a str>,
    statuses: &'a [i32],
    /// Every replaced function also writes a line to its output stream, as a real command might.
    noisy: bool,
}

const SEED_NAME: &str = "devvm-0a1b2c3d";

impl Default for Rename<'_> {
    fn default() -> Self {
        Rename {
            computer: "DEVVM",
            meta: Some(r#"{"local-hostname": "devvm-0a1b2c3d"}"#),
            marker: None,
            statuses: &[0],
            noisy: false,
        }
    }
}

/// `call` run, in PowerShell 7, against the functions of the boot task as shipped (`Restart-Now` and
/// `Set-ComputerNameFromSeed`), with everything that changes the system replaced by a function that logs its call:
/// `Rename-Computer`, the flush of the name, `Write-Whole`, `Send-Line`, `Start-Sleep` and `shutdown.exe` (a function
/// outranks the application of that name). Returns the log, one entry per call, then `call`'s own entries; an error of `call` is the
/// entry `threw:<message>`. `$disk` and `$marker` are the paths the script reads.
fn boot_task(start: &Rename, call: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let script_path = dir.path().join("boot-task.ps1");
    let files = answer_disk_files(&inputs()).unwrap();
    let task = files.iter().find(|file| file.name == "boot-task.ps1").unwrap();
    std::fs::write(&script_path, &task.bytes).unwrap();
    let disk = dir.path().join("disk");
    std::fs::create_dir(&disk).unwrap();
    if let Some(meta) = start.meta {
        std::fs::write(disk.join("meta.json"), meta).unwrap();
    }
    let marker = dir.path().join("renamed-to");
    if let Some(name) = start.marker {
        std::fs::write(&marker, name).unwrap();
    }
    let statuses = start.statuses.iter().map(i32::to_string).collect::<Vec<_>>().join(",");
    let script = r#"$ErrorActionPreference = 'Stop'
$log = [Collections.Generic.List[string]]::new()
$statuses = [Collections.Generic.Queue[int]]::new([int[]]@(@@STATUSES@@))
$tokens = $null; $errors = $null
$tree = [Management.Automation.Language.Parser]::ParseFile(@@TASK@@, [ref]$tokens, [ref]$errors)
if ($errors) { throw "the boot task does not parse: $($errors | Out-String)" }
$noisy = @@NOISY@@
function Stray { if ($noisy) { 'stray output' } }
$shipped = 'Restart-Now', 'Set-ComputerNameFromSeed'
$found = @($tree.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] }, $false))
foreach ($function in $found) { if ($function.Name -in $shipped) { Invoke-Expression $function.Extent.Text } }
if (@($found | Where-Object { $_.Name -in $shipped }).Count -ne $shipped.Count) { throw "the boot task lacks one of: $shipped" }
function Rename-Computer { param([string]$NewName, [switch]$Force) $log.Add("rename:$NewName"); Stray }
function Save-ComputerName { $log.Add('flush'); Stray }
function Write-Whole([string]$Target, [byte[]]$Bytes, [string[]]$ReadSids = @()) {
    $log.Add('marker:' + [Text.Encoding]::UTF8.GetString($Bytes))
    [IO.File]::WriteAllBytes($Target, $Bytes)
    Stray
}
function Send-Line([string]$Text) { $log.Add("line:$Text"); Stray }
function Start-Sleep { param([int]$Milliseconds) $log.Add('sleep') }
function shutdown.exe {
    $log.Add("shutdown:$args")
    Stray
    if ($args[0] -eq '/a') { $global:LASTEXITCODE = 0; return }
    if ($statuses.Count -eq 0) { throw 'the stub ran out of statuses' }
    $global:LASTEXITCODE = $statuses.Dequeue()
}
$env:COMPUTERNAME = @@COMPUTER@@
$disk = @@DISK@@
$marker = @@MARKER@@
try { @@CALL@@ } catch { $log.Add("threw:$($_.Exception.Message)") }
$log
"#
    .replace("@@STATUSES@@", &statuses)
    .replace("@@NOISY@@", if start.noisy { "$true" } else { "$false" })
    .replace("@@TASK@@", &literal(&script_path.display().to_string()))
    .replace("@@COMPUTER@@", &literal(start.computer))
    .replace("@@DISK@@", &literal(&disk.display().to_string()))
    .replace("@@MARKER@@", &literal(&marker.display().to_string()))
    // Last, so the call is never scanned for a placeholder.
    .replace("@@CALL@@", call);
    let output = run(&script, "none", None);
    assert_eq!(output.status.code(), Some(0), "{}", shown(&output));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

const DECIDE: &str = r#"$log.Add("returned:$(Set-ComputerNameFromSeed $disk $marker)")"#;
const RESTART: &str = "Restart-Now; $log.Add('returned')";

#[skuld::test]
fn a_computer_that_has_the_seeds_name_is_left_alone(#[fixture(pwsh_wrap)] _group: &Group) {
    let log = boot_task(
        &Rename {
            computer: SEED_NAME,
            ..Rename::default()
        },
        DECIDE,
    );
    assert_eq!(log, ["returned:False"]);
}

#[skuld::test]
fn a_computer_whose_name_differs_from_the_seeds_only_in_case_is_left_alone(#[fixture(pwsh_wrap)] _group: &Group) {
    // Windows reports a computer name in capitals.
    let log = boot_task(
        &Rename {
            computer: "DEVVM-0A1B2C3D",
            ..Rename::default()
        },
        DECIDE,
    );
    assert_eq!(log, ["returned:False"]);
}

#[skuld::test]
fn the_decision_is_one_bool_whatever_the_commands_it_runs_write_to_their_output(#[fixture(pwsh_wrap)] _group: &Group) {
    let log = boot_task(
        &Rename {
            noisy: true,
            ..Rename::default()
        },
        r#"$result = @(Set-ComputerNameFromSeed $disk $marker); $log.Add("count:$($result.Count) $($result[0] -is [bool]) $($result[0])")"#,
    );
    assert_eq!(log.last().map(String::as_str), Some("count:1 True True"), "{log:?}");
}

#[skuld::test]
fn a_computer_that_has_another_name_is_renamed_flushed_recorded_and_restarted(#[fixture(pwsh_wrap)] _group: &Group) {
    // A marker of another name (an earlier rename) does not stop the rename.
    for marker in [None, Some("devvm-ffffffff")] {
        let log = boot_task(
            &Rename {
                marker,
                ..Rename::default()
            },
            DECIDE,
        );
        assert_eq!(
            log,
            [
                &format!("rename:{SEED_NAME}"),
                "flush",
                &format!("marker:{SEED_NAME}"),
                &format!("line:DEVVM-RENAMED={SEED_NAME}"),
                "shutdown:/r /f /t 0",
                "returned:True",
            ],
            "marker {marker:?}"
        );
    }
}

#[skuld::test]
fn a_rename_that_did_not_take_effect_fails_naming_the_rename_instead_of_restarting_again(
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let log = boot_task(
        &Rename {
            marker: Some(SEED_NAME),
            ..Rename::default()
        },
        DECIDE,
    );
    assert_eq!(
        log,
        [format!(
            "threw:the computer is still named DEVVM after the rename to {SEED_NAME} and a restart"
        )]
    );
}

#[skuld::test]
fn a_seed_without_a_local_hostname_fails_naming_it(#[fixture(pwsh_wrap)] _group: &Group) {
    for meta in ["{}", r#"{"local-hostname": ""}"#] {
        let log = boot_task(
            &Rename {
                meta: Some(meta),
                ..Rename::default()
            },
            DECIDE,
        );
        assert_eq!(log, ["threw:the seed's meta.json has no local-hostname"], "{meta}");
    }
}

#[skuld::test]
fn a_restart_is_requested_again_while_it_is_refused_as_not_ready_and_each_refusal_is_logged_once(
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let log = boot_task(
        &Rename {
            statuses: &[21, 1722, 21, 1190, 1722, 1190, 0],
            ..Rename::default()
        },
        RESTART,
    );
    let request = "shutdown:/r /f /t 0";
    let refused = |status: i32| format!("line:DEVVM-RESTART-REFUSED={status}");
    // Each refusal is followed by a wait before the next request; 1190 aborts the scheduled shutdown first.
    assert_eq!(
        log,
        [
            request,
            &refused(21),
            "sleep",
            request,
            &refused(1722),
            "sleep",
            request,
            "sleep",
            request,
            &refused(1190),
            "shutdown:/a",
            "sleep",
            request,
            "sleep",
            request,
            "shutdown:/a",
            "sleep",
            request,
            "returned",
        ]
    );
}

#[skuld::test]
fn a_shutdown_already_in_progress_counts_as_a_restart_that_was_accepted(#[fixture(pwsh_wrap)] _group: &Group) {
    let log = boot_task(
        &Rename {
            statuses: &[1115],
            ..Rename::default()
        },
        RESTART,
    );
    assert_eq!(log, ["shutdown:/r /f /t 0", "returned"]);
}

#[skuld::test]
fn any_other_refusal_of_the_restart_ends_the_boot_naming_the_status(#[fixture(pwsh_wrap)] _group: &Group) {
    let log = boot_task(
        &Rename {
            statuses: &[21, 5],
            ..Rename::default()
        },
        RESTART,
    );
    assert_eq!(
        log,
        [
            "shutdown:/r /f /t 0",
            "line:DEVVM-RESTART-REFUSED=21",
            "sleep",
            "shutdown:/r /f /t 0",
            "threw:shutdown.exe /r ended with status 5",
        ]
    );
}

// The elevated helper's runner ========================================================================================

/// Compiles the native helper into `dir` as the bootstrap does, so a rendered runner there can load it.
fn compile_native_helper(dir: &std::path::Path) {
    let source = dir.join(NATIVE_SOURCE);
    let files = answer_disk_files(&inputs()).unwrap();
    let native = files.iter().find(|file| file.name == NATIVE_SOURCE).unwrap();
    std::fs::write(&source, &native.bytes).unwrap();
    let compile = format!(
        "$ErrorActionPreference = 'Stop'; Add-Type -TypeDefinition (Get-Content -LiteralPath {} -Raw) -OutputAssembly {}",
        literal(&source.display().to_string()),
        literal(&dir.join(NATIVE_DLL).display().to_string())
    );
    let built = run_windows_powershell(&compile);
    assert_eq!(built.status.code(), Some(0), "{}", shown(&built));
}

/// The native helper, compiled once per test run: compiling takes seconds, and every runner test needs it.
struct NativeHelper(tempfile::TempDir);

#[skuld::fixture(scope = process)]
fn native_helper(#[fixture(pwsh_wrap)] _group: &Group) -> Result<NativeHelper, String> {
    let dir = tempfile::tempdir().map_err(|e| format!("cannot create a temp dir: {e}"))?;
    compile_native_helper(dir.path());
    Ok(NativeHelper(dir))
}

/// The runner as the answer disk has it, with `root` as the requests' root, written to `dir/eb-runner.ps1` beside a
/// copy of the native helper, where the runner finds it.
fn write_runner(helper: &NativeHelper, dir: &std::path::Path, root: &std::path::Path) -> std::path::PathBuf {
    std::fs::copy(helper.0.path().join(NATIVE_DLL), dir.join(NATIVE_DLL)).unwrap();
    let mut values = super::values(&inputs());
    values.iter_mut().find(|(name, _)| *name == "EB_ROOT").unwrap().1 = root.display().to_string();
    let files = disk_files(
        &[TEMPLATES
            .iter()
            .find(|(name, _)| *name == "eb-runner.ps1")
            .copied()
            .unwrap()],
        &values,
    )
    .unwrap();
    let path = dir.join("eb-runner.ps1");
    std::fs::write(&path, &files[0].bytes).unwrap();
    path
}

/// What a request's caller file says in [`run_runner`].
#[derive(Clone, Copy)]
enum Caller {
    /// The process that runs the test script, which is alive.
    Alive,
    /// The id of that process with another start time: ended.
    Ended,
    /// No caller file.
    Missing,
}

/// The id of a request that is not in the root: a runner started for it only sweeps.
const NO_REQUEST: &str = "00000000000000000000000000000000";

/// The id of a request a runner is started for.
const REQUEST: &str = "abcdefabcdefabcdefabcdefabcdefab";

/// A file of the request `request` that is open to nobody else until the removal first pauses.
struct Held<'a> {
    request: &'a str,
    file: &'a str,
}

/// What [`run_runner`] saw.
struct RunnerRun {
    /// The names left in the root, sorted.
    left: Vec<String>,
    /// The names in the held request at the removal's first pause, sorted: where the removal that failed on the held file
    /// had got to. Empty if it never paused.
    at_first_pause: Vec<String>,
}

/// A runner started for the request `id` when the root held `requests`: each a name, its caller and the other files it
/// holds. The runner runs in the test's own PowerShell, whose `Start-Sleep` is replaced by a function: with `held`, the
/// file is open to nobody else until the first pause, and the pause says what the request holds then.
fn run_runner(helper: &NativeHelper, id: &str, requests: &[(&str, Caller, &[&str])], held: Option<Held>) -> RunnerRun {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("eb");
    let runner = write_runner(helper, dir.path(), &root);
    let mut script = format!(
        "$ErrorActionPreference = 'Stop'
$root = {}
$self = [Diagnostics.Process]::GetCurrentProcess()
function Make([string]$name, $caller, [string[]]$files) {{
    $d = Join-Path $root $name
    [void][IO.Directory]::CreateDirectory($d)
    foreach ($file in $files) {{ [IO.File]::WriteAllText((Join-Path $d $file), '') }}
    if ($null -ne $caller) {{ [IO.File]::WriteAllText((Join-Path $d 'caller'), $caller) }}
}}
",
        literal(&root.display().to_string())
    );
    for (name, caller, files) in requests {
        let caller = match caller {
            Caller::Alive => "\"$($self.Id) $($self.StartTime.ToUniversalTime().Ticks)\"",
            Caller::Ended => "\"$($self.Id) 1\"",
            Caller::Missing => "$null",
        };
        let files = files.iter().map(|file| literal(file)).collect::<Vec<_>>().join(", ");
        script.push_str(&format!("Make {} {caller} @({files})\n", literal(name)));
    }
    if let Some(Held { request, file }) = held {
        script.push_str(&format!(
            "$global:requestDir = Join-Path $root {}
$global:held = [IO.File]::Open((Join-Path $global:requestDir {}), 'Open', 'Read', 'None')
function global:Start-Sleep {{ param($Milliseconds)
    if ($global:held) {{
        $names = Get-ChildItem -LiteralPath $global:requestDir -Force | ForEach-Object Name | Sort-Object
        [Console]::Out.WriteLine('PAUSE ' + ($names -join ' '))
        $global:held.Dispose(); $global:held = $null
    }}
}}
",
            literal(request),
            literal(file)
        ));
    }
    script.push_str(&format!(
        "$global:LASTEXITCODE = 0
& {} {}
if ($LASTEXITCODE -ne 0) {{ throw \"the runner ended with $LASTEXITCODE\" }}
Get-ChildItem -LiteralPath $root -Force | ForEach-Object Name | Sort-Object
",
        literal(&runner.display().to_string()),
        literal(id)
    ));
    let output = run(&script, "none", None);
    assert_eq!(output.status.code(), Some(0), "{}", shown(&output));
    let (mut left, mut at_first_pause) = (Vec::new(), Vec::new());
    for line in String::from_utf8_lossy(&output.stdout).lines().map(str::trim) {
        match line.strip_prefix("PAUSE") {
            Some(names) => at_first_pause = names.split_whitespace().map(str::to_owned).collect(),
            None => left.push(line.to_owned()),
        }
    }
    RunnerRun { left, at_first_pause }
}

/// [`run_runner`] with nothing held: the names left in the root.
fn after_runner(helper: &NativeHelper, id: &str, requests: &[(&str, Caller, &[&str])]) -> Vec<String> {
    run_runner(helper, id, requests, None).left
}

#[skuld::test]
fn the_runners_sweep_removes_requests_and_unpublished_requests_whose_caller_ended_and_no_runner_holds(
    #[fixture(native_helper)] helper: &NativeHelper,
) {
    let left = after_runner(
        helper,
        NO_REQUEST,
        &[
            ("ended", Caller::Ended, &["script.ps1"]),
            ("ended.tmp", Caller::Ended, &["script.ps1"]),
            // What a caller leaves when it withdrew the request: the claim it made itself, which nobody holds.
            ("withdrawn.tmp", Caller::Ended, &["script.ps1", "claim"]),
            ("alive", Caller::Alive, &["script.ps1"]),
            ("alive.tmp", Caller::Alive, &["script.ps1"]),
            // Made but not yet given its caller: a caller still building it, or one that died in between, which nothing
            // tells apart.
            ("building.tmp", Caller::Missing, &["script.ps1"]),
        ],
    );
    assert_eq!(left, ["alive", "alive.tmp", "building.tmp"]);
}

#[skuld::test]
fn the_runner_removes_the_request_it_served_as_its_last_act(#[fixture(native_helper)] helper: &NativeHelper) {
    // The caller ended, so the runner serves nothing and answers nobody: it claims the request and removes it.
    assert_eq!(
        after_runner(
            helper,
            REQUEST,
            &[(REQUEST, Caller::Ended, &["script.ps1"]), ("alive", Caller::Alive, &[])]
        ),
        ["alive"]
    );
}

#[skuld::test]
fn the_sweep_takes_a_published_request_whose_caller_file_is_gone(#[fixture(native_helper)] helper: &NativeHelper) {
    // What a removal that stopped part way leaves: no caller file reads as an ended caller.
    let left = after_runner(
        helper,
        NO_REQUEST,
        &[
            ("partly", Caller::Missing, &["script.ps1", "stdout", "claim"]),
            ("emptied", Caller::Missing, &[]),
        ],
    );
    assert_eq!(left, Vec::<String>::new());
}

#[skuld::test]
fn a_late_runner_on_a_request_whose_caller_and_claim_are_gone_removes_it_and_does_not_wait(
    #[fixture(native_helper)] helper: &NativeHelper,
) {
    // The state a removal leaves just before it ends the directory: the runner has no caller to wait on.
    assert_eq!(
        after_runner(helper, REQUEST, &[(REQUEST, Caller::Missing, &["script.ps1"])]),
        Vec::<String>::new()
    );
}

#[skuld::test]
fn a_late_runner_on_a_request_whose_claim_is_there_leaves_it(#[fixture(native_helper)] helper: &NativeHelper) {
    // The claim a caller made to withdraw the request, or a runner's: the request is not this runner's.
    assert_eq!(
        after_runner(helper, REQUEST, &[(REQUEST, Caller::Alive, &["script.ps1", "claim"])]),
        [REQUEST]
    );
}

#[skuld::test]
fn the_sweep_removes_the_caller_and_the_claim_after_every_other_file(#[fixture(native_helper)] helper: &NativeHelper) {
    let run = run_runner(
        helper,
        NO_REQUEST,
        &[(
            "swept",
            Caller::Ended,
            &["claim", "done", "exit", "read", "script.ps1", "stdout"],
        )],
        Some(Held {
            request: "swept",
            file: "stdout",
        }),
    );
    // The removal that failed on `stdout` had removed the files before it, and neither `caller` nor `claim`.
    assert_eq!(run.at_first_pause, ["caller", "claim", "stdout"]);
    assert_eq!(run.left, Vec::<String>::new());
}

#[skuld::test]
fn the_runners_last_removal_removes_the_caller_and_the_claim_after_every_other_file(
    #[fixture(native_helper)] helper: &NativeHelper,
) {
    let run = run_runner(
        helper,
        REQUEST,
        &[(
            REQUEST,
            Caller::Ended,
            &["done", "exit", "read", "script.ps1", "stdout"],
        )],
        Some(Held {
            request: REQUEST,
            file: "stdout",
        }),
    );
    // The runner makes its own claim and closes it before the removal.
    assert_eq!(run.at_first_pause, ["caller", "claim", "stdout"]);
    assert_eq!(run.left, Vec::<String>::new());
}
