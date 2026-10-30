use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::{script, wrap};
use crate::host::Host;
use crate::test_support::host::TestHost;
use crate::{Command, ErrorKind, RunAs};

fn cmd(argv: &[&str]) -> Command {
    Command::new(argv.iter().copied())
}

/// `text` as one single-quoted word, written independently of the code under test.
fn sq(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

// Golden strings ======================================================================================================

#[skuld::test]
fn wrap_applies_cwd_env_and_elevation_in_order() {
    let mut user = cmd(&["echo", "a b"]);
    user.cwd = Some("/w".to_owned());
    user.env = vec![("K".to_owned(), "v'x".to_owned()), ("L".to_owned(), String::new())];
    let user_script = r"cd '/w' && export K='v'\''x' L='' && exec -- 'echo' 'a b'";
    assert_eq!(script(&user).unwrap(), user_script);
    assert_eq!(wrap(&user).unwrap(), format!("bash -lc {}", sq(user_script)));

    let mut elevated = user.clone();
    elevated.run_as = RunAs::Elevated;
    let elevated_script = format!("exec sudo -n -- bash -c {}", sq(user_script));
    assert_eq!(script(&elevated).unwrap(), elevated_script);
    assert_eq!(wrap(&elevated).unwrap(), format!("bash -lc {}", sq(&elevated_script)));
}

#[skuld::test]
fn cd_and_export_appear_only_when_asked_for() {
    assert_eq!(script(&cmd(&["ls"])).unwrap(), "exec -- 'ls'");
    assert_eq!(wrap(&cmd(&["ls"])).unwrap(), r"bash -lc 'exec -- '\''ls'\'''");
}

#[skuld::test]
fn wrap_keeps_env_and_cwd_inside_the_sudo_shell() {
    let mut elevated = cmd(&["id"]);
    elevated.run_as = RunAs::Elevated;
    elevated.cwd = Some("/root".to_owned());
    elevated.env = vec![("A".to_owned(), "1".to_owned())];

    let script = script(&elevated).unwrap();
    let (before_inner, inner) = script
        .split_once("bash -c ")
        .expect("the root shell is started by `bash -c`, never `sh -c`");
    assert_eq!(before_inner, "exec sudo -n -- ", "nothing runs before sudo");
    assert_eq!(inner, sq("cd '/root' && export A='1' && exec -- 'id'"));
}

#[skuld::test]
fn an_empty_argv_and_a_bad_env_name_are_usage_errors() {
    let mut empty = cmd(&[]);
    empty.cwd = Some("/".to_owned());
    let error = wrap(&empty).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Usage);
    assert!(error.message().contains("argv"), "{}", error.message());

    for name in ["", "1A", "A-B", "A B", "A=B", "A\n", "\u{e9}", "A'"] {
        let mut bad = cmd(&["true"]);
        bad.env = vec![(name.to_owned(), "v".to_owned())];
        let error = wrap(&bad).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Usage, "{name:?}");
        assert!(error.message().contains("environment"), "{}", error.message());
    }
    for name in ["A", "_", "_a1", "Zz_9"] {
        let mut good = cmd(&["true"]);
        good.env = vec![(name.to_owned(), "v".to_owned())];
        wrap(&good).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[skuld::test]
fn a_nul_byte_cannot_travel_in_a_shell_string() {
    let nul = "a\0b";
    let mut in_argv = cmd(&["echo", nul]);
    in_argv.run_as = RunAs::Elevated;
    let mut in_cwd = cmd(&["echo"]);
    in_cwd.cwd = Some(nul.to_owned());
    let mut in_value = cmd(&["echo"]);
    in_value.env = vec![("K".to_owned(), nul.to_owned())];
    for command in [in_argv, in_cwd, in_value] {
        let error = wrap(&command).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(error.message().contains("NUL"), "{}", error.message());
    }
}

// A real shell ========================================================================================================

/// The `bash` the tests start, found from git rather than `PATH`: on Windows a bare `bash` can be the WSL launcher in
/// `System32`, which prints a UTF-16 message and exits 1 when no distribution is installed. It is the first `bin/bash`
/// in a directory above `git --exec-path` (Git for Windows: `<Git>/bin/bash.exe`; elsewhere `/usr/bin/bash` or
/// `/bin/bash`). Panics naming what was looked for.
fn bash_program() -> PathBuf {
    let mut git = std::process::Command::new("git");
    git.arg("--exec-path");
    TestHost.quiet(&mut git);
    let output = git
        .output()
        .unwrap_or_else(|e| panic!("these tests need `git` on PATH, to find its `bash`: {e}"));
    assert!(
        output.status.success(),
        "`git --exec-path` failed: {:?} {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let exec_path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let bash = format!("bin/bash{}", std::env::consts::EXE_SUFFIX);
    exec_path
        .ancestors()
        .map(|dir| dir.join(&bash))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| {
            panic!(
                "these tests need git's `bash`: no `{bash}` above `git --exec-path` ({})",
                exec_path.display()
            )
        })
}

/// Runs `script` as the standard input of the `bash -s` of [`bash_program`], with `path_first` (if any) at the front of
/// `PATH` and `LC_ALL=C.UTF-8`, so neither the host's code page nor its locale can change the bytes. The script travels
/// as bytes on stdin, never as an argument.
fn run_in_bash(script: &str, path_first: Option<&Path>) -> std::process::Output {
    let mut command = std::process::Command::new(bash_program());
    command
        .arg("-s")
        .env("LC_ALL", "C.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = path_first {
        let mut dirs = vec![dir.to_path_buf()];
        dirs.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
        command.env("PATH", std::env::join_paths(dirs).unwrap());
    }
    TestHost.quiet(&mut command);
    let mut child = command
        .spawn()
        .unwrap_or_else(|e| panic!("this test needs to start git's `bash`: {e}"));
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{script}\n").as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// A script `name` in `dir`, executable, that runs `body` with bash.
fn write_script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/bash\n{body}\n")).unwrap();
    TestHost.set_executable(&path).unwrap();
}

#[skuld::test]
fn wrap_round_trips_hostile_arguments_through_a_real_sh() {
    let hostile = [
        "",
        "a\nb",
        "it's",
        "\"quoted\"",
        "$x",
        "`id`",
        "%",
        "^",
        "&",
        ";",
        "\u{e9} \u{416} \u{4e16} \u{1f600}",
        "-x",
        "a=b",
        "  lead and trail  ",
        "\\n",
    ];
    let mut argv = vec!["printf", r"%s\0"];
    argv.extend(hostile);

    let out = run_in_bash(&wrap(&cmd(&argv)).unwrap(), None);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut seen: Vec<&[u8]> = out.stdout.split(|byte| *byte == 0).collect();
    assert_eq!(seen.pop(), Some(&b""[..]), "every argument ends with a NUL");
    let expected: Vec<&[u8]> = hostile.iter().map(|arg| arg.as_bytes()).collect();
    assert_eq!(seen, expected);
}

#[skuld::test]
fn a_program_called_dash_x_or_a_equals_b_is_a_program_name() {
    for name in ["-x", "a=b"] {
        let out = run_in_bash(&wrap(&cmd(&[name])).unwrap(), None);
        let stderr = String::from_utf8_lossy(&out.stderr);
        // 127 is "no such program"; an option would give 2, and an assignment would leave `exec` with no program.
        assert_eq!(out.status.code(), Some(127), "{name}: {stderr}");
        assert!(stderr.contains(name), "{name}: {stderr}");
    }
}

#[skuld::test]
fn elevated_wrapper_runs_end_to_end_with_a_stub_sudo() {
    let stubs = skuld::TempDir::new().unwrap();
    write_script(&stubs, "sudo", "shift 2\nexec \"$@\"");
    // A hostile program name: a space, a quote and a dollar sign.
    let program = "we ird's $name";
    write_script(&stubs, program, r#"printf '%s|%s|%s' "$PWD" "$MARK" "$1""#);

    let mut elevated = cmd(&[program, "an arg; $x"]);
    elevated.run_as = RunAs::Elevated;
    elevated.cwd = Some("/usr".to_owned());
    elevated.env = vec![("MARK".to_owned(), "it's".to_owned())];

    // The full string starts a login shell, whose profile may reset `PATH` and hide the stub; the golden tests cover
    // the `-l`. This runs the script it carries.
    let out = run_in_bash(&script(&elevated).unwrap(), Some(&stubs));
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "/usr|it's|an arg; $x");
}
