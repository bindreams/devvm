//! `wrap`'s scripts in a real PowerShell 7 on the test host (the `pwsh_wrap` group: it spawns `pwsh`, which the CI
//! Windows Test lane and the deputy's runner do and a developer's PC may not). The host runs the script as the broker's
//! `pwsh.exe -c <script>` does, with the test binary as the guest program, in its helper modes below.
//!
//! The boundary of the command line the guest's sshd and broker build is not measured here: the host's `pwsh` has
//! neither of them (the guest tests measure it).

use std::path::{Path, PathBuf};
use std::process::{Command as Process, Output, Stdio};

use super::wrap_tests::{hint, longest_fit};
use super::{LINE_LIMIT, wrap};
use crate::guest::GuestOs;
use crate::guest::windows::WindowsGuest;
use crate::recipe::Shell;
use crate::test_groups::{Group, pwsh_wrap};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::helper::HELPER_VAR;
use crate::{Command, ErrorKind};

// Running a script ====================================================================================================

pub(in crate::guest::windows) fn test_binary() -> String {
    std::env::current_exe().unwrap().display().to_string()
}

/// `script` in `pwsh -c`, the test binary in `helper` mode, `path` (a directory) first on the PATH.
pub(in crate::guest::windows) fn run(script: &str, helper: &str, path: Option<&Path>) -> Output {
    let search = path.map_or(Search::Caller, Search::First);
    run_with("pwsh", script, Some(helper), search)
}

/// Where the script finds programs.
pub(in crate::guest::windows) enum Search<'a> {
    Caller,
    First(&'a Path),
    /// A PATH of its own: the directory (if any), then the directory of `pwsh.exe` and the system directory, and
    /// nothing the caller has (Git's `usr\bin` has a `dir.exe` and a `mkdir.exe`, which are programs); and a PATHEXT
    /// that is a subset of the stock one (`.COM;.EXE;.BAT;.CMD`), so it has no `.PS1`.
    Only(Option<&'a Path>),
    /// [`Search::Only`] with no directory, and `modules` first on the `PSModulePath`.
    Modules(&'a Path),
}

/// [`run`] in the PowerShell `exe` (`powershell.exe` is Windows PowerShell 5.1, which the guest's own scripts run in),
/// with no helper variable when `helper` is `None`, and the programs found by `search`.
pub(in crate::guest::windows) fn run_with(exe: &str, script: &str, helper: Option<&str>, search: Search) -> Output {
    let mut process = process(exe, script, helper);
    match search {
        Search::Caller => {}
        Search::First(dir) => {
            let caller = std::env::var_os("PATH").unwrap();
            let paths = std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&caller));
            process.env("PATH", std::env::join_paths(paths).unwrap());
        }
        Search::Only(first) => only(&mut process, first),
        Search::Modules(modules) => {
            only(&mut process, None);
            let caller = std::env::var_os("PSModulePath").unwrap_or_default();
            let paths = std::iter::once(modules.to_path_buf()).chain(std::env::split_paths(&caller));
            process.env("PSModulePath", std::env::join_paths(paths).unwrap());
        }
    }
    process.output().unwrap_or_else(|error| panic!("{exe} runs: {error}"))
}

/// The PATH and PATHEXT of [`Search::Only`].
fn only(process: &mut Process, first: Option<&Path>) {
    let caller = std::env::var_os("PATH").unwrap();
    // An entry is read as `Find-Program` reads it: without its double quotes.
    let pwsh_dir = std::env::split_paths(&caller)
        .map(|dir| PathBuf::from(dir.to_string_lossy().trim_matches('"')))
        .find(|dir| dir.join("pwsh.exe").is_file())
        .expect("pwsh.exe is on the PATH");
    let system = Path::new(&std::env::var_os("SystemRoot").unwrap()).join("System32");
    let paths = first.map(Path::to_path_buf).into_iter().chain([pwsh_dir, system]);
    process.env("PATH", std::env::join_paths(paths).unwrap());
    process.env("PATHEXT", ".COM;.EXE;.BAT;.CMD");
}

/// The process of `script` in `exe`, with the helper variable of `helper` (none when it is `None`).
fn process(exe: &str, script: &str, helper: Option<&str>) -> Process {
    let mut process = Process::new(exe);
    process
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdin(Stdio::null());
    match helper {
        Some(helper) => process.env(HELPER_VAR, helper),
        None => process.env_remove(HELPER_VAR),
    };
    process
}

pub(in crate::guest::windows) fn shown(output: &Output) -> String {
    format!(
        "status {:?}, stdout {:?}, stderr {:?}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The text of hex digits `hex` wrote.
fn unhex(digits: &str) -> String {
    let bytes: Vec<u8> = (0..digits.len() / 2)
        .map(|i| u8::from_str_radix(&digits[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    String::from_utf8(bytes).unwrap()
}

/// The arguments `argv_echo` printed.
fn echoed(output: &Output) -> Vec<String> {
    let text = String::from_utf8(output.stdout.clone()).unwrap_or_else(|_| panic!("{}", shown(output)));
    text.lines()
        .filter_map(|line| line.strip_prefix("ARG"))
        .map(|line| unhex(line.split_once('=').unwrap().1))
        .collect()
}

pub(in crate::guest::windows) fn wrapped(argv: &[&str]) -> String {
    wrap(&Command::new(argv.iter().copied())).unwrap()
}

// The tests ===========================================================================================================

pub(in crate::guest::windows) const HOSTILE: &[&str] = &[
    "",
    " ",
    "a b",
    "'",
    "''",
    "\"",
    "\"\"",
    "\\",
    "\\\\",
    "a\\",
    "a\\\"b",
    "\\\"",
    "$x",
    "$(boom)",
    "`",
    "`n",
    "%PATH%",
    "--%",
    "-x",
    "a=b",
    "\u{e9}\u{4e16}",
    "\u{1f600}",
    "\u{2018}",
    "\u{2019}",
    "\u{201a}",
    "\u{201b}",
    "a\nb",
    "a\tb",
    "*",
    "?",
    "[a]",
    "&",
    "|",
    ";",
    "<",
    ">",
    "(",
    ")",
    "{",
    "}",
    "@",
    "#",
    "!",
    "^",
    ",",
];

#[skuld::test]
fn wrap_round_trips_hostile_arguments_through_a_real_pwsh(#[fixture(pwsh_wrap)] _group: &Group) {
    let exe = test_binary();
    let mut together = vec![exe.as_str()];
    together.extend(HOSTILE);
    let output = run(&wrapped(&together), "wrap-argv", None);
    assert_eq!(echoed(&output), HOSTILE, "{}", shown(&output));
    // The ones that decide how a literal is written, one at a time.
    for hostile in [
        "--%", "'", "\u{2018}", "\u{2019}", "\u{201a}", "\u{201b}", "a\\\"b", "%PATH%",
    ] {
        let output = run(&wrapped(&[&exe, hostile]), "wrap-argv", None);
        assert_eq!(echoed(&output), [hostile], "{hostile:?}: {}", shown(&output));
    }
}

#[skuld::test]
fn a_windows_program_called_dash_x_or_a_equals_b_is_a_program_name(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let bin = home.join("bin");
    std::fs::create_dir(&bin).unwrap();
    for name in ["-x.exe", "a=b.exe"] {
        // A copy: the test binary may be on another drive than `home`, which a hard link cannot cross.
        std::fs::copy(test_binary(), bin.join(name)).unwrap();
    }
    for name in ["-x", "a=b"] {
        let output = run(&wrapped(&[name, "ok"]), "wrap-argv", Some(&bin));
        assert_eq!(echoed(&output), ["ok"], "{name}: {}", shown(&output));
    }
    let output = run(&wrapped(&["no-such-program-xyz"]), "wrap-argv", None);
    assert_eq!(output.status.code(), Some(127), "{}", shown(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no-such-program-xyz: command not found"));
    for name in ["Get-ChildItem", "dir", "mkdir"] {
        let output = run_with("pwsh", &wrapped(&[name]), Some("wrap-argv"), Search::Only(None));
        assert_eq!(output.status.code(), Some(127), "{name}: {}", shown(&output));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&hint(name)),
            "{name}: {}",
            shown(&output)
        );
    }
    let output = run_with(
        "pwsh",
        &wrapped(&["pwsh", "-c", "Get-Location"]),
        Some("wrap-argv"),
        Search::Only(None),
    );
    assert_eq!(output.status.code(), Some(0), "{}", shown(&output));
    // A wildcard in the name matches no command, and a name that is none is not found.
    for name in ["Get-ChildIte[m]", "Get-*", "*"] {
        let output = run_with("pwsh", &wrapped(&[name]), Some("wrap-argv"), Search::Only(None));
        assert_eq!(output.status.code(), Some(127), "{name}: {}", shown(&output));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("devvm: {name}: command not found")),
            "{name}: {}",
            shown(&output)
        );
    }
}

#[skuld::test]
fn a_script_gets_the_hint_and_the_scripts_own_function_gets_none(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    // `.ps1` is not in the PATHEXT of a stock Windows, so a script is not a program but `pwsh -c` runs it.
    let scripts = home.join("scripts");
    std::fs::create_dir(&scripts).unwrap();
    for name in ["tool.ps1", "my script.ps1"] {
        std::fs::write(scripts.join(name), "'ran'").unwrap();
    }
    let hinted = |output: &Output, name: &str| {
        assert_eq!(output.status.code(), Some(127), "{}", shown(output));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&hint(name)),
            "{}",
            shown(output)
        );
    };
    hinted(
        &run_with(
            "pwsh",
            &wrapped(&["tool"]),
            Some("wrap-argv"),
            Search::Only(Some(&scripts)),
        ),
        "tool",
    );
    hinted(
        &run_with(
            "pwsh",
            &wrapped(&["my script"]),
            Some("wrap-argv"),
            Search::Only(Some(&scripts)),
        ),
        "my script",
    );
    // Named by a path, in which `[`, `]` and a backtick are not a pattern: relative to the working directory, and
    // absolute.
    let named = scripts.join("proj[1]");
    std::fs::create_dir(&named).unwrap();
    for name in ["a[1].ps1", "b`c.ps1", "TOOL.PS1", "note.txt"] {
        std::fs::write(scripts.join(name), "'ran'").unwrap();
    }
    std::fs::write(named.join("tool.ps1"), "'ran'").unwrap();
    let by_path = |name: &str| {
        let mut command = Command::new([name]);
        command.cwd = Some(scripts.display().to_string());
        run_with("pwsh", &wrap(&command).unwrap(), Some("wrap-argv"), Search::Only(None))
    };
    let absolute = named.join("tool.ps1").display().to_string();
    for name in [
        r".\tool.ps1",
        r".\a[1].ps1",
        r".\b`c.ps1",
        "./TOOL.PS1",
        r".\proj[1]\tool.ps1",
        absolute.as_str(),
    ] {
        hinted(&by_path(name), name);
    }
    // A name by path that is no script, or a script that is not there, is not found.
    for name in [r".\note.txt", r".\a[2].ps1", r".\proj[1]"] {
        let output = by_path(name);
        assert_eq!(output.status.code(), Some(127), "{name}: {}", shown(&output));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&format!("devvm: {name}: command not found")),
            "{name}: {}",
            shown(&output)
        );
    }
    // The advice works: `pwsh -c` runs the script of PATH.
    let output = run_with(
        "pwsh",
        &wrapped(&["pwsh", "-c", "tool"]),
        Some("wrap-argv"),
        Search::Only(Some(&scripts)),
    );
    assert_eq!(output.status.code(), Some(0), "{}", shown(&output));
    // The script's output ends it: a profile of the PC that runs the test may print before.
    assert!(
        String::from_utf8_lossy(&output.stdout).trim_end().ends_with("ran"),
        "{}",
        shown(&output)
    );
    let output = run_with(
        "pwsh",
        &wrapped(&["Find-Program"]),
        Some("wrap-argv"),
        Search::Only(None),
    );
    assert_eq!(output.status.code(), Some(127), "{}", shown(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("devvm: Find-Program: command not found"),
        "{}",
        shown(&output)
    );
}

/// A module of `PSModulePath` is where a command that is neither a file of PATH nor a script comes from.
#[skuld::test]
fn a_filter_of_a_module_gets_the_hint_and_a_module_that_will_not_load_is_not_found(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let modules = home.join("modules");
    for (module, text) in [
        (
            "Filt",
            "filter Invoke-Filt { $_ }\nExport-ModuleMember -Function Invoke-Filt\n",
        ),
        ("Broken", "throw 'does not load'\n"),
    ] {
        std::fs::create_dir_all(modules.join(module)).unwrap();
        std::fs::write(modules.join(module).join(format!("{module}.psm1")), text).unwrap();
    }
    let output = run_with(
        "pwsh",
        &wrapped(&["Invoke-Filt"]),
        Some("wrap-argv"),
        Search::Modules(&modules),
    );
    assert_eq!(output.status.code(), Some(127), "{}", shown(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&hint("Invoke-Filt")),
        "{}",
        shown(&output)
    );
    // Measured: a module that throws does not make `Get-Command` throw, so the name is not found.
    let output = run_with(
        "pwsh",
        &wrapped(&["Invoke-Nothing"]),
        Some("wrap-argv"),
        Search::Modules(&modules),
    );
    assert_eq!(output.status.code(), Some(127), "{}", shown(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("devvm: Invoke-Nothing: command not found"),
        "{}",
        shown(&output)
    );
}

#[skuld::test]
fn a_lookup_that_throws_is_127_and_the_message_says_so(#[fixture(pwsh_wrap)] _group: &Group) {
    // A `Get-Command` of the user's that throws, in front of the script.
    let script = format!(
        "function Get-Command {{ throw 'no lookup' }}\n{}",
        wrapped(&["Get-Date"])
    );
    let output = run_with("pwsh", &script, Some("wrap-argv"), Search::Only(None));
    assert_eq!(output.status.code(), Some(127), "{}", shown(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .starts_with("devvm: Get-Date: command not found (lookup failed: no lookup)"),
        "{}",
        shown(&output)
    );
}

#[skuld::test]
fn a_status_over_i32_max_survives_a_real_pwsh(#[fixture(pwsh_wrap)] _group: &Group) {
    let exe = test_binary();
    for code in [
        0u32,
        7,
        125,
        255,
        256,
        0xC000_0005,
        0xFFFF_FFFF,
        0x8000_0000,
        0x7FFF_FFFF,
    ] {
        let output = run(&wrapped(&[&exe, &code.to_string()]), "wrap-exit", None);
        assert_eq!(output.status.code(), Some(code as i32), "{code:#x}: {}", shown(&output));
    }
}

#[skuld::test]
fn stdout_bytes_above_0x7f_and_crlf_pass_unchanged(#[fixture(pwsh_wrap)] _group: &Group) {
    let exe = test_binary();
    let output = run(&wrapped(&[&exe, "all256"]), "wrap-bytes", None);
    assert_eq!(output.stdout, (0..=255u8).collect::<Vec<_>>(), "stdout");
    assert_eq!(output.stderr, (0..=255u8).rev().collect::<Vec<_>>(), "stderr");
    let output = run(&wrapped(&[&exe, "crlf"]), "wrap-bytes", None);
    assert_eq!(output.stdout, b"a\r\nb\nc\r\n\r\nlast");
    assert_eq!(output.stderr, b"e1\r\ne2\n");
}

#[skuld::test]
fn wrap_sets_the_working_directory_of_the_program(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let exe = test_binary();
    for name in ["plain", "dir with space", "[brackets]", "it's", "\u{e9}\u{4e16}"] {
        let dir = home.join(name);
        std::fs::create_dir(&dir).unwrap();
        let mut command = Command::new([exe.as_str()]);
        command.cwd = Some(dir.display().to_string());
        let output = run(&wrap(&command).unwrap(), "wrap-argv", None);
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let cwd = text
            .lines()
            .find_map(|line| line.strip_prefix("CWD="))
            .unwrap_or_default()
            .to_owned();
        // The same directory by identity: the program reports the long spelling of a path given in the 8.3 one
        // (`RUNNER~1`), which is the directory's own.
        assert_eq!(
            std::fs::canonicalize(unhex(&cwd)).unwrap(),
            std::fs::canonicalize(&dir).unwrap(),
            "{name}: {}",
            shown(&output)
        );
    }
    let mut missing = Command::new([exe.as_str()]);
    missing.cwd = Some(home.join("does-not-exist").display().to_string());
    // A directory that cannot be entered is a command that did not run: 126 and the reason, not the status of a failed
    // script.
    let output = run(&wrap(&missing).unwrap(), "wrap-argv", None);
    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(output.stdout.is_empty(), "{}", shown(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("devvm: working directory "),
        "{}",
        shown(&output)
    );
}

#[skuld::test]
fn a_program_that_cannot_start_exits_126_naming_it(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let bad = home.join("bad.exe");
    std::fs::write(&bad, "this is not a program").unwrap();
    let bad = bad.display().to_string();
    let output = run(&wrapped(&[&bad, "x"]), "wrap-argv", None);
    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(output.stdout.is_empty(), "{}", shown(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&format!("devvm: {bad}: ")),
        "{}",
        shown(&output)
    );
}

/// `[`, `]` and a backtick are wildcard syntax and an escape for PowerShell, and ordinary characters of a file name.
#[skuld::test]
fn a_program_is_found_by_its_literal_path_or_name(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let bin = home.join("tools[x86] `a`");
    std::fs::create_dir(&bin).unwrap();
    for name in ["w[1].exe", "tick`.exe"] {
        std::fs::copy(test_binary(), bin.join(name)).unwrap();
    }
    for typed in ["w[1].exe", "w[1]", "tick`"] {
        let path = bin.join(typed).display().to_string();
        let output = run(&wrapped(&[&path, "ok"]), "wrap-argv", None);
        assert_eq!(echoed(&output), ["ok"], "{path}: {}", shown(&output));
        let output = run(&wrapped(&[typed, "ok"]), "wrap-argv", Some(&bin));
        assert_eq!(echoed(&output), ["ok"], "{typed} on the PATH: {}", shown(&output));
    }
    // A file the brackets would match as a wildcard is not found by a name that only matches it so.
    let output = run(&wrapped(&["w[1-9]", "ok"]), "wrap-argv", Some(&bin));
    assert_eq!(output.status.code(), Some(127), "{}", shown(&output));
}

/// The 18 cases measured on a guest where the other carriers differ: the arguments of a `.cmd` file, and `cmd /c`.
#[skuld::test]
fn a_cmd_file_target_keeps_cmd_quoting(#[fixture(unicode_home)] home: &Path, #[fixture(pwsh_wrap)] _group: &Group) {
    let exe = test_binary();
    let script = home.join("showargs.cmd");
    std::fs::write(&script, format!("@echo off\r\n\"{exe}\" %*\r\n")).unwrap();
    let script = script.display().to_string();
    for arg in [
        "a b",
        "q\"uote",
        "a&b",
        "100%",
        "^caret",
        "a|b",
        "(paren)",
        "trailing\\",
        "",
        "plain",
        "two \"q\" here",
        "a;b,c=d",
        "!bang!",
    ] {
        let output = run(&wrapped(&[&script, arg]), "wrap-argv", None);
        assert_eq!(echoed(&output), [arg], "{arg:?}: {}", shown(&output));
    }
    // A batch argument std refuses is refused, and nothing runs.
    let output = run(&wrapped(&[&script, "a\nb"]), "wrap-argv", None);
    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(output.stdout.is_empty(), "{}", shown(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("line break"),
        "{}",
        shown(&output)
    );

    let quoted = format!("\"{exe}\" \"a b\" \"c&d\" \"q\"\"x\"");
    for (script, stdout, status) in [
        ("echo hello", Some("hello"), 0),
        ("echo a & echo b", Some("a\r\nb"), 0),
        ("exit /b 3", Some(""), 3),
        (quoted.as_str(), None, 0),
        ("echo \"x\" && exit /b 4", Some("\"x\""), 4),
    ] {
        let output = run(&wrapped(&["cmd.exe", "/d", "/c", script]), "wrap-argv", None);
        assert_eq!(output.status.code(), Some(status), "{script}: {}", shown(&output));
        match stdout {
            // `echo a & echo b` writes the space before the `&` too.
            Some(want) => assert_eq!(
                String::from_utf8_lossy(&output.stdout)
                    .replace(" \r\n", "\r\n")
                    .trim_end(),
                want,
                "{script}"
            ),
            None => assert_eq!(echoed(&output), ["a b", "c&d", "q\"x"], "{script}: {}", shown(&output)),
        }
    }
    // A text of several lines is refused, and nothing runs.
    let lines = run(&wrapped(&["cmd.exe", "/d", "/c", "echo a\necho b"]), "wrap-argv", None);
    assert_eq!(lines.status.code(), Some(126), "{}", shown(&lines));
    assert!(lines.stdout.is_empty(), "{}", shown(&lines));
    assert!(
        String::from_utf8_lossy(&lines.stderr).contains("line break"),
        "{}",
        shown(&lines)
    );
    // `/c` ends a chain of switches, and is a plain argument of a program that is not cmd.
    let chained = run(&wrapped(&["cmd.exe", "/d/c", "echo hello"]), "wrap-argv", None);
    assert_eq!(chained.status.code(), Some(0), "{}", shown(&chained));
    assert_eq!(String::from_utf8_lossy(&chained.stdout).trim_end(), "hello");
    let other = run(&wrapped(&[exe.as_str(), "/c", "a b"]), "wrap-argv", None);
    assert_eq!(echoed(&other), ["/c", "a b"], "{}", shown(&other));
    // A `/c` stuck to other text is refused, and nothing runs.
    let stuck = run(&wrapped(&["cmd.exe", "/cecho hi"]), "wrap-argv", None);
    assert_eq!(stuck.status.code(), Some(126), "{}", shown(&stuck));
    assert!(stuck.stdout.is_empty(), "{}", shown(&stuck));
    // The words after `/c` stay one each when they are a program and its arguments.
    let words = run(
        &wrapped(&["cmd.exe", "/d", "/c", exe.as_str(), "a b", "c&d", "q\"x"]),
        "wrap-argv",
        None,
    );
    assert_eq!(echoed(&words), ["a b", "c&d", "q\"x"], "{}", shown(&words));
}

#[skuld::test]
fn shell_argv_for_pwsh_writes_non_ascii_as_utf8(#[fixture(pwsh_wrap)] _group: &Group) {
    // Through the wrapper: a nested `pwsh` is a native program, and its console output encoding is the OEM page
    // unless the first statement set it.
    let argv = WindowsGuest
        .shell_argv(Some(Shell::Pwsh), "Write-Output 'é世😀'")
        .unwrap();
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let output = run(&wrapped(&argv), "wrap-argv", None);
    assert_eq!(output.stdout, "é世😀\r\n".as_bytes(), "{}", shown(&output));
}

#[skuld::test]
fn a_pwsh_step_whose_first_statement_must_come_first_runs_and_keeps_its_status(#[fixture(pwsh_wrap)] _group: &Group) {
    for (script, stdout, status) in [
        (
            "using namespace System.IO\n[Path]::GetFileName('a/b.txt')",
            "b.txt\r\n",
            0,
        ),
        ("param()\nWrite-Output 'ok'", "ok\r\n", 0),
        ("Write-Output 'before'\nexit 3", "before\r\n", 3),
    ] {
        let argv = WindowsGuest.shell_argv(Some(Shell::Pwsh), script).unwrap();
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let output = run(&wrapped(&argv), "wrap-argv", None);
        assert_eq!(output.stdout, stdout.as_bytes(), "{script:?}: {}", shown(&output));
        assert_eq!(output.status.code(), Some(status), "{script:?}: {}", shown(&output));
    }
}

#[skuld::test]
fn a_line_at_the_limit_runs_and_one_over_is_refused(#[fixture(pwsh_wrap)] _group: &Group) {
    let exe = test_binary();
    let pad = |n: usize| Command::new([exe.clone(), "x".repeat(n)]);
    let low = longest_fit(&pad, LINE_LIMIT);
    let output = run(&wrap(&pad(low)).unwrap(), "wrap-argv", None);
    assert_eq!(
        echoed(&output),
        ["x".repeat(low)],
        "{} units: status {:?}",
        low,
        output.status.code()
    );
    let error = wrap(&pad(low + 1)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Usage, "{error}");
}
