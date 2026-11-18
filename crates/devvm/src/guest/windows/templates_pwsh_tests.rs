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
    let source = dir.path().join(NATIVE_SOURCE);
    let dll = dir.path().join(NATIVE_DLL);
    let files = answer_disk_files(&inputs()).unwrap();
    let native = files.iter().find(|file| file.name == NATIVE_SOURCE).unwrap();
    std::fs::write(&source, &native.bytes).unwrap();

    // Windows PowerShell 5.1, as the first-logon script does: C#, written to an assembly on disk.
    let compile = format!(
        "$ErrorActionPreference = 'Stop'; Add-Type -TypeDefinition (Get-Content -LiteralPath {} -Raw) -OutputAssembly {}",
        literal(&source.display().to_string()),
        literal(&dll.display().to_string())
    );
    let built = run_windows_powershell(&compile);
    assert_eq!(built.status.code(), Some(0), "{}", shown(&built));
    assert!(dll.is_file(), "no assembly was written: {}", shown(&built));

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
