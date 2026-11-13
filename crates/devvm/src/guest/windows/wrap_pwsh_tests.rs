//! `wrap`'s scripts in a real PowerShell 7 on the test host (the `pwsh_wrap` group: it spawns `pwsh`, which the CI
//! Windows Test lane and the deputy's runner do and a developer's PC may not). The host runs the script as the broker's
//! `pwsh.exe -c <script>` does, with the test binary as the guest program, in its helper modes below.
//!
//! The boundary of the command line the guest's sshd and broker build is not measured here: the host's `pwsh` has
//! neither of them (the guest tests measure it).

use std::path::Path;
use std::process::{Command as Process, Output, Stdio};

use super::{LINE_LIMIT, command_line_lengths, script_unchecked, wrap};
use crate::guest::GuestOs;
use crate::guest::windows::WindowsGuest;
use crate::recipe::Shell;
use crate::test_groups::{Group, pwsh_wrap};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::helper::HELPER_VAR;
use crate::test_support::wrapped_programs::hex;
use crate::{Command, ErrorKind};

// Running a script ====================================================================================================

fn test_binary() -> String {
    std::env::current_exe().unwrap().display().to_string()
}

/// `script` in `pwsh -c`, the test binary in `helper` mode, `path` (a directory) first on the PATH.
fn run(script: &str, helper: &str, path: Option<&Path>) -> Output {
    let mut process = Process::new("pwsh");
    process
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env(HELPER_VAR, helper)
        .stdin(Stdio::null());
    if let Some(dir) = path {
        let mut paths = vec![dir.to_path_buf()];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        process.env("PATH", std::env::join_paths(paths).unwrap());
    }
    process.output().expect("pwsh runs")
}

fn shown(output: &Output) -> String {
    format!(
        "status {:?}, stdout {:?}, stderr {:?}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The arguments `argv_echo` printed.
fn echoed(output: &Output) -> Vec<String> {
    let text = String::from_utf8(output.stdout.clone()).unwrap_or_else(|_| panic!("{}", shown(output)));
    text.lines()
        .filter_map(|line| line.strip_prefix("ARG"))
        .map(|line| {
            let (_, digits) = line.split_once('=').unwrap();
            let bytes: Vec<u8> = (0..digits.len() / 2)
                .map(|i| u8::from_str_radix(&digits[2 * i..2 * i + 2], 16).unwrap())
                .collect();
            String::from_utf8(bytes).unwrap()
        })
        .collect()
}

fn wrapped(argv: &[&str]) -> String {
    wrap(&Command::new(argv.iter().copied())).unwrap()
}

// The tests ===========================================================================================================

const HOSTILE: &[&str] = &[
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
        std::fs::hard_link(test_binary(), bin.join(name)).unwrap();
    }
    for name in ["-x", "a=b"] {
        let output = run(&wrapped(&[name, "ok"]), "wrap-argv", Some(&bin));
        assert_eq!(echoed(&output), ["ok"], "{name}: {}", shown(&output));
    }
    let output = run(&wrapped(&["no-such-program-xyz"]), "wrap-argv", None);
    assert_eq!(output.status.code(), Some(127), "{}", shown(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no-such-program-xyz: command not found"));
    // A cmdlet's name is not a program.
    let output = run(&wrapped(&["Get-ChildItem"]), "wrap-argv", None);
    assert_eq!(output.status.code(), Some(127), "{}", shown(&output));
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
        assert_eq!(cwd, hex(&dir.display().to_string()), "{name}: {}", shown(&output));
    }
    let mut missing = Command::new([exe.as_str()]);
    missing.cwd = Some(home.join("does-not-exist").display().to_string());
    assert_ne!(run(&wrap(&missing).unwrap(), "wrap-argv", None).status.code(), Some(0));
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
fn a_line_at_the_limit_runs_and_one_over_is_refused(#[fixture(pwsh_wrap)] _group: &Group) {
    let exe = test_binary();
    let pad = |n: usize| Command::new([exe.clone(), "x".repeat(n)]);
    let fits = |n: usize| {
        let (sshd, broker) = command_line_lengths(&script_unchecked(&pad(n)).unwrap());
        sshd.max(broker) <= LINE_LIMIT
    };
    let (mut low, mut high) = (0, LINE_LIMIT);
    while low + 1 < high {
        let middle = (low + high) / 2;
        if fits(middle) { low = middle } else { high = middle }
    }
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
