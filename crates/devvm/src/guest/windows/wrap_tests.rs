//! Golden tests of the script `wrap` builds. What the script does in a real PowerShell is checked in
//! `wrap_pwsh_tests.rs`.

use super::{LINE_LIMIT, command_line_lengths, wrap};
use crate::{Command, ErrorKind, RunAs};

fn command(argv: &[&str]) -> Command {
    Command::new(argv.iter().copied())
}

fn script(argv: &[&str]) -> String {
    wrap(&command(argv)).unwrap()
}

#[skuld::test]
fn wrap_takes_the_c2_form_for_programs_and_the_legacy_form_for_batch_targets() {
    // Quotes in all four spellings PowerShell reads as one, `--%`, a line break and a trailing backslash.
    let hostile = [
        "it's",
        "\u{2018}a\u{2019}b\u{201a}c\u{201b}",
        "--%",
        "a b",
        "x\\",
        "e\u{e9}\u{4e16}",
    ];
    let mut argv = vec!["tool"];
    argv.extend(hostile);
    let text = script(&argv);
    // The C2 form: one `ArgumentList.Add` per argument, a quote written doubled.
    assert!(text.contains("$psi.ArgumentList.Add('it''s')"), "{text}");
    assert!(
        text.contains("$psi.ArgumentList.Add('\u{2018}\u{2018}a\u{2019}\u{2019}b\u{201a}\u{201a}c\u{201b}\u{201b}')"),
        "{text}"
    );
    assert!(text.contains("$psi.ArgumentList.Add('--%')"), "{text}");
    assert!(text.contains("$psi.ArgumentList.Add('x\\')"), "{text}");
    assert!(text.contains("$psi.ArgumentList.Add('e\u{e9}\u{4e16}')"), "{text}");
    // The legacy form is in the script too, in std's batch quoting, and the guest picks by the file it found.
    assert!(
        text.contains(r#"'/e:ON /v:OFF /d /c ""' + $app.Source + '"'"#),
        "{text}"
    );
    assert!(text.contains(r#"' "it''s" "#), "{text}");
    assert!(text.contains(r#""a b""#), "{text}");
    assert!(
        text.contains("$ext -ieq '.cmd'") && text.contains("$ext -ieq '.bat'"),
        "{text}"
    );
    // The program is started by the file the lookup found, never by the name typed.
    assert!(
        text.contains("[Diagnostics.ProcessStartInfo]::new($app.Source)"),
        "{text}"
    );
}

#[skuld::test]
fn a_cmd_command_gets_the_command_line_form_for_cmd_exe_only() {
    let text = script(&["cmd", "/d", "/c", "echo hi & echo \"x\""]);
    assert!(text.contains(r#"-ieq 'cmd.exe'"#), "{text}");
    assert!(text.contains(r#"'/d /c "echo hi & echo "x"""#), "{text}");
    // Without `/c` text there is no such form.
    assert!(!script(&["cmd", "/d"]).contains("-ieq 'cmd.exe'"));
}

#[skuld::test]
fn wrap_resolves_argv0_as_an_application_never_a_cmdlet() {
    let text = script(&["Get-ChildItem", "x"]);
    assert!(
        text.contains("Get-Command -CommandType Application -Name ([Management.Automation.WildcardPattern]::Escape('Get-ChildItem'))"),
        "{text}"
    );
    assert!(
        !text.contains("& $app") && !text.contains("Invoke-Expression"),
        "{text}"
    );
}

#[skuld::test]
fn wrap_sets_the_working_directory_literally() {
    let mut cmd = command(&["tool"]);
    cmd.cwd = Some("C:\\a b\\it's [x]*".to_owned());
    let text = wrap(&cmd).unwrap();
    assert!(
        text.contains("Set-Location -LiteralPath 'C:\\a b\\it''s [x]*'"),
        "{text}"
    );
    assert!(
        text.contains("$psi.WorkingDirectory = (Get-Location).ProviderPath"),
        "{text}"
    );
    // The directory comes before the lookup, as `cd` does before `exec` on Linux: a relative argv[0] is found from it.
    assert!(
        text.find("Set-Location").unwrap() < text.find("Get-Command").unwrap(),
        "{text}"
    );
    // No directory asked for: the command starts where the session starts.
    assert!(!script(&["tool"]).contains("Set-Location"));
    assert!(!script(&["tool"]).contains("WorkingDirectory"));
}

#[skuld::test]
fn wrap_exits_127_naming_a_missing_program() {
    let text = script(&["no such tool"]);
    assert!(
        text.contains(
            "if (-not $app) { [Console]::Error.WriteLine('devvm: no such tool: command not found'); exit 127 }"
        ),
        "{text}"
    );
}

#[skuld::test]
fn wrap_starts_with_a_session_check_that_exits_126_in_session_0() {
    let mut cmd = command(&["tool"]);
    cmd.cwd = Some("C:\\x".to_owned());
    for run_as in [RunAs::User, RunAs::Elevated] {
        cmd.run_as = run_as;
        let text = wrap(&cmd).unwrap();
        let check = text.find("SessionId -eq 0").unwrap();
        assert!(
            text[check..].starts_with(
                "SessionId -eq 0) { [Console]::Error.WriteLine('devvm: the command did not reach the user''s session'); exit 126 }"
            ),
            "{text}"
        );
        // Before the directory, the lookup, the helper request and everything else that has an effect.
        for later in ["Set-Location", "Get-Command", "CreateDirectory", "Run("] {
            if let Some(at) = text.find(later) {
                assert!(check < at, "{later} comes before the session check in {text}");
            }
        }
    }
}

#[skuld::test]
fn wrap_sets_no_output_encoding() {
    for run_as in [RunAs::User, RunAs::Elevated] {
        let mut cmd = command(&["tool", "\u{e9}"]);
        cmd.run_as = run_as;
        let text = wrap(&cmd).unwrap();
        assert!(!text.contains("OutputEncoding"), "{text}");
    }
}

#[skuld::test]
fn wrap_refuses_an_empty_argv_a_nul_and_an_oversize_line() {
    let refused = |cmd: &Command| {
        let error = wrap(cmd).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Usage, "{error}");
        error
    };
    assert!(refused(&Command::default()).message().contains("argv"));
    let mut empty_cwd = command(&["tool"]);
    empty_cwd.cwd = Some(String::new());
    assert!(refused(&empty_cwd).message().contains("working directory"));
    assert!(refused(&command(&["tool", "a\0b"])).message().contains("NUL"));
    let mut nul_cwd = command(&["tool"]);
    nul_cwd.cwd = Some("a\0b".to_owned());
    assert!(refused(&nul_cwd).message().contains("NUL"));

    // The line is over when the larger of the two lines is, and the error names both and the limit. Padding the one
    // argument by one character at a time finds the edge from the lengths `wrap` itself computes.
    let pad = |n: usize| command(&["tool", &"x".repeat(n)]);
    let fits = |n: usize| {
        let text = super::script_unchecked(&pad(n)).unwrap();
        let (sshd, broker) = command_line_lengths(&text);
        sshd.max(broker) <= LINE_LIMIT
    };
    let (mut low, mut high) = (0, LINE_LIMIT);
    while low + 1 < high {
        let middle = (low + high) / 2;
        if fits(middle) { low = middle } else { high = middle }
    }
    assert!(wrap(&pad(low)).is_ok());
    let error = refused(&pad(low + 1));
    let (sshd, broker) = command_line_lengths(&super::script_unchecked(&pad(low + 1)).unwrap());
    for number in [sshd, broker, LINE_LIMIT] {
        assert!(
            error.message().contains(&number.to_string()),
            "{number} is not in {error}"
        );
    }
}

#[skuld::test]
fn the_two_command_lines_are_counted_in_utf16_units() {
    // Astral characters are two units each, and a quote or a backslash run before one costs more: the escaped line
    // is longer than the script.
    let plain = command_line_lengths("a b");
    let astral = command_line_lengths("a \u{1f600}");
    assert_eq!(astral.0, plain.0 + 1);
    assert_eq!(astral.1, plain.1 + 1);
    let quoted = command_line_lengths("a \"b\"");
    assert!(quoted.0 > plain.0 + 3 && quoted.1 > plain.1 + 3);
}

#[skuld::test]
fn a_batch_argument_std_refuses_is_refused_in_the_script() {
    let text = script(&["tool", "a\nb"]);
    // The batch form is the refusal; the other forms are untouched, so a program that is not a batch file still gets
    // the newline.
    assert!(
        text.contains("[Console]::Error.WriteLine('devvm: argument 1 contains a line break"),
        "{text}"
    );
    assert!(text.contains("exit 126"), "{text}");
    assert!(text.contains("$psi.ArgumentList.Add('a\nb')"), "{text}");
}

#[skuld::test]
fn wrap_of_an_elevated_command_calls_the_helper() {
    let mut cmd = command(&["tool", "it's"]);
    cmd.run_as = RunAs::Elevated;
    cmd.cwd = Some("C:\\x".to_owned());
    let text = wrap(&cmd).unwrap();
    // The request: a directory under the helper's root, built under a temporary name and then renamed, holding the
    // command as the script a Medium wrapper would run.
    assert!(text.contains("C:\\ProgramData\\devvm\\eb"), "{text}");
    assert!(text.contains("script.ps1") && text.contains("caller"), "{text}");
    assert!(text.contains("Move"), "{text}");
    // The inner script is the user's command, quoted once as a literal.
    assert!(text.contains("it''''s"), "{text}");
    // One start per request, through the scheduled task.
    assert!(text.contains("'devvm-eb'") && text.contains(".Run("), "{text}");
    // The streams are copied as the files grow, and the status is the helper's.
    assert!(text.contains("'stdout'") && text.contains("'stderr'"), "{text}");
    assert!(text.contains("'exit'") && text.contains("'done'"), "{text}");
    // A user command's script has none of it.
    assert!(!script(&["tool"]).contains("devvm-eb"));
}
