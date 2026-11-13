//! The answer disk's scripts and helper in real PowerShells (the `pwsh_wrap` group, which the CI Windows Test lane and
//! the deputy's runner run; a developer's PC may not). The text tests of `templates_tests.rs` cannot see a syntax error
//! or a compile error; these can. What the scripts do on a guest is measured on a guest.

use std::process::{Command as Process, Output, Stdio};

use super::{NATIVE_DLL, NATIVE_SOURCE, TEMPLATES, answer_disk_files, disk_files};
use crate::guest::windows::templates::templates_tests::inputs;
use crate::guest::windows::wrap::literal;
use crate::guest::windows::wrap::wrap_pwsh_tests::{HOSTILE, run, shown};
use crate::test_groups::{Group, pwsh_wrap};

/// The parse errors of the script `text` in this PowerShell's grammar, one per line, with their line numbers.
fn parse_errors(dir: &std::path::Path, name: &str, text: &[u8]) -> Output {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    let script = format!(
        "$errors = $null; $tokens = $null\n\
         [void][System.Management.Automation.Language.Parser]::ParseFile({}, [ref]$tokens, [ref]$errors)\n\
         foreach ($e in $errors) {{ \"$($e.Extent.StartLineNumber): $($e.Message)\" }}",
        literal(&path.display().to_string())
    );
    run(&script, "none", None)
}

#[skuld::test]
fn every_script_of_the_answer_disk_parses_as_powershell(#[fixture(pwsh_wrap)] _group: &Group) {
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<_> = answer_disk_files(&inputs())
        .unwrap()
        .into_iter()
        .filter(|file| file.name.ends_with(".ps1"))
        .collect();
    assert_eq!(scripts.len(), 4, "bootstrap, boot-task, eb-runner and lib");
    for file in scripts {
        let output = parse_errors(dir.path(), &file.name, &file.bytes);
        assert_eq!(output.status.code(), Some(0), "{}: {}", file.name, shown(&output));
        let errors = String::from_utf8_lossy(&output.stdout);
        assert!(errors.trim().is_empty(), "{} does not parse:\n{errors}", file.name);
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
    let built = Process::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &compile])
        .stdin(Stdio::null())
        .output()
        .expect("powershell.exe runs");
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
