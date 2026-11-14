//! Golden tests of the script `wrap` builds. What the script does in a real PowerShell is checked in
//! `wrap_pwsh_tests.rs`.

use super::{LINE_LIMIT, NOT_IN_SESSION, command_line_lengths, literal, session_check, wrap};
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
    assert!(text.contains(r#"'/e:ON /v:OFF /d /c ""' + $app + '"'"#), "{text}");
    assert!(text.contains(r#"' "it''s" "#), "{text}");
    assert!(text.contains(r#""a b""#), "{text}");
    assert!(
        text.contains("$ext -ieq '.cmd'") && text.contains("$ext -ieq '.bat'"),
        "{text}"
    );
    // The program is started by the file the lookup found, never by the name typed.
    assert!(text.contains("[Diagnostics.ProcessStartInfo]::new($app)"), "{text}");
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
fn a_program_that_does_not_name_cmd_carries_no_cmd_command_line() {
    // The host decides by argv[0]: another program's `/c` is one of its arguments, written once.
    let text = script(&["tool.exe", "/c", "file with spaces"]);
    assert!(!text.contains("-ieq 'cmd.exe'"), "{text}");
    assert_eq!(text.matches("file with spaces").count(), 2, "{text}");
}

#[skuld::test]
fn wrap_resolves_argv0_as_an_application_never_a_cmdlet() {
    let text = script(&["Get-ChildItem", "x"]);
    assert!(text.contains("$app = Find-Program 'Get-ChildItem'\n"), "{text}");
    // The name is looked up as the file it is: PATH and PATHEXT, with no wildcard reading of `[` or `*`.
    assert!(text.contains("[IO.File]::Exists("), "{text}");
    assert!(
        !text.contains("Get-Command") && !text.contains("WildcardPattern"),
        "{text}"
    );
    assert!(
        !text.contains("& $app") && !text.contains("Invoke-Expression"),
        "{text}"
    );
}

#[skuld::test]
fn a_program_that_cannot_start_and_a_directory_that_cannot_be_entered_exit_126_naming_what_they_are() {
    let mut cmd = command(&["it's a tool"]);
    cmd.cwd = Some("C:/x".to_owned());
    let text = wrap(&cmd).unwrap();
    assert!(
        text.contains(
            "try { Set-Location -LiteralPath 'C:/x' } catch { [Console]::Error.WriteLine('devvm: working directory C:/x: ' + $_.Exception.Message); exit 126 }"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "try { $p = [Diagnostics.Process]::Start($psi) } catch { [Console]::Error.WriteLine('devvm: it''s a tool: ' + $_.Exception.Message); exit 126 }"
        ),
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
        text.find("Set-Location").unwrap() < text.find("$app = Find-Program").unwrap(),
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
        for later in ["Set-Location", "Find-Program", "CreateDirectory", "Run("] {
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

    // The line is over when the larger of the two lines is, and the error names both and the limit.
    let pad = |n: usize| command(&["tool", &"x".repeat(n)]);
    let low = longest_fit(&pad);
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

/// `CreateProcess` takes 32,767 units of command line including the terminating NUL, so a line of 32,767 units is
/// refused: a guest's sshd refused the script whose longer line was exactly that (the guest test of the limit).
#[skuld::test]
fn a_line_of_the_limit_plus_the_terminating_null_is_refused() {
    let padded = |n: usize| {
        let text = format!("'ok' # {}", "a".repeat(n));
        command(&["pwsh", "-NoProfile", "-NonInteractive", "-Command", &text])
    };
    let longer = |n: usize| {
        let (sshd, broker) = command_line_lengths(&super::script_unchecked(&padded(n)).unwrap());
        sshd.max(broker)
    };
    let (mut low, mut high) = (0, 40_000);
    while low + 1 < high {
        let middle = (low + high) / 2;
        if longer(middle) <= 32_767 {
            low = middle
        } else {
            high = middle
        }
    }
    assert_eq!(longer(low), 32_767, "a pad of {low} makes a line of 32,767 units");
    let error = wrap(&padded(low)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Usage, "{error}");
    assert!(wrap(&padded(low - 1)).is_ok(), "a line of 32,765 units is taken");
}

/// The most that `make(n)` can be padded by (`n` characters of its argument) and still fit both command lines, found
/// from the lengths `wrap` computes, one character at a time.
pub(super) fn longest_fit(make: &dyn Fn(usize) -> Command) -> usize {
    let fits = |n: usize| {
        let (sshd, broker) = command_line_lengths(&super::script_unchecked(&make(n)).unwrap());
        sshd.max(broker) <= LINE_LIMIT
    };
    let (mut low, mut high) = (0, LINE_LIMIT);
    while low + 1 < high {
        let middle = (low + high) / 2;
        if fits(middle) { low = middle } else { high = middle }
    }
    low
}

/// What the edge test of the lengths cannot do: they are counted by the code under test there. Here they are counted by
/// hand, for a script with a quote, a backslash before a character and a backslash at its end.
#[skuld::test]
fn the_lines_of_a_small_script_are_counted_by_hand() {
    // `a "b" c\d\` is 10 units. It has a space, so it is one quoted argument (2 units); each of its two quotes gets a
    // backslash (2) and its last backslash is doubled before the closing quote (1): 15 units. The broker line is
    // `C:\ProgramData\ssh-broker\ssh-broker.exe` (40) and ` -c `; the PowerShell line is
    // `"C:\Program Files\PowerShell\7\pwsh.exe"` (38 and two quotes) and ` -c `.
    assert_eq!(command_line_lengths(r#"a "b" c\d\"#), (40 + 4 + 15, 38 + 2 + 4 + 15));
}

#[skuld::test]
fn the_limit_holds_for_an_elevated_command_and_for_an_argument_that_quoting_makes_longer() {
    let edge = |run_as: RunAs, filler: &'static str| {
        let make = move |n: usize| {
            let mut cmd = command(&["tool", &filler.repeat(n)]);
            cmd.run_as = run_as;
            cmd
        };
        let low = longest_fit(&make);
        assert!(wrap(&make(low)).is_ok(), "{run_as:?} {filler}: {low} fits");
        let error = wrap(&make(low + 1)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Usage, "{run_as:?} {filler}: {error}");
        low
    };
    let user = edge(RunAs::User, "x");
    // The helper's script holds the user's as a literal inside its own, so less fits.
    assert!(edge(RunAs::Elevated, "x") < user);
    // A quote costs two units on each command line, and one more in the script that carries the helper's request.
    assert!(edge(RunAs::User, "\"") < user / 2 + 1);
    assert!(edge(RunAs::Elevated, "\"") < edge(RunAs::Elevated, "x") / 2 + 1);
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

#[skuld::test]
fn the_session_check_prints_the_message_the_readiness_probe_matches() {
    let check = session_check();
    assert!(check.contains(&literal(&format!("devvm: {NOT_IN_SESSION}"))), "{check}");
    assert!(check.ends_with("exit 126 }\n"), "{check}");
}

#[skuld::test]
fn an_elevated_script_checks_its_session_where_the_caller_and_the_inner_script_both_run() {
    let mut cmd = command(&["tool"]);
    cmd.run_as = RunAs::Elevated;
    let text = wrap(&cmd).unwrap();
    // The caller's check is the first statement after the preference line.
    assert!(
        text.starts_with(&format!("$ErrorActionPreference = 'Stop'\n{}", session_check())),
        "{text}"
    );
    // The inner script keeps its own check (a literal, so each quote is doubled): the helper's runner must run in the
    // user's interactive session, or the command ends with 126 there.
    let inner = session_check().replace('\'', "''");
    assert_eq!(text.matches(&inner).count(), 1, "{text}");
}

#[skuld::test]
fn a_multi_line_cmd_text_is_refused_in_the_script_like_any_argument() {
    for text in ["echo a\necho b", "echo a\r\necho b"] {
        let script = script(&["cmd", "/d", "/c", text]);
        assert!(
            script.contains("[Console]::Error.WriteLine('devvm: argument 3 contains a line break"),
            "{script}"
        );
        // No file is written and nothing is cleaned up: the command is not run.
        assert!(
            !script.contains("WriteAllBytes") && !script.contains("finally"),
            "{script}"
        );
    }
}

#[skuld::test]
fn a_line_break_in_one_word_of_a_cmd_command_is_refused_in_the_script() {
    let text = script(&["cmd", "/d", "/c", "echo", "a\nb"]);
    assert!(
        text.contains("[Console]::Error.WriteLine('devvm: argument 4 contains a line break"),
        "{text}"
    );
    assert!(!text.contains("WriteAllBytes"), "{text}");
}

#[skuld::test]
fn the_words_of_a_cmd_command_are_quoted_one_by_one_in_the_script() {
    let text = script(&["cmd", "/d", "/c", "dir", r"C:\Program Files"]);
    assert!(text.contains(r#"'/d /c "dir "C:\Program Files"""#), "{text}");
}
