//! The caller script of an elevated command in a real PowerShell 7 (the `pwsh_wrap` group), against a stand-in for the
//! scheduled task and its runner. The runner's side of the contract is plain files, so a few lines of PowerShell play
//! it; the real task and runner are measured on a guest. The file names below are literals on purpose: they are the
//! runner's contract, and renaming a constant must not rename them here.

use std::path::{Path, PathBuf};
use std::process::Output;

use super::{ROOT, caller_script, literal};
use crate::guest::windows::wrap::wrap;
use crate::guest::windows::wrap::wrap_pwsh_tests::{run, shown, stderr};
use crate::guest::windows::wrap::wrap_tests::hint;
use crate::test_groups::{Group, pwsh_wrap};
use crate::test_support::fixtures::unicode_home;
use crate::{Command, RunAs};

/// [`standin_for`] the caller script of the command `inner`.
fn standin(root: &Path, run: &str, refresh: &str, state: &str) -> String {
    standin_for(&caller_script("inner"), root, run, refresh, state)
}

/// `text` (a caller script) with the scheduled task and the requests' root replaced. `run` is the body of the task's
/// `Run` (it has the request's id as `$id` and the root as `$root`); it starts the instance whose `Refresh` runs the
/// body `refresh` and whose `State` is read by the body `state`.
/// The task's own `State` is running, as it is while another request's instance runs: nothing in the caller may take it
/// for its instance's.
fn standin_for(text: &str, root: &Path, run: &str, refresh: &str, state: &str) -> String {
    let task = format!(
        "$started = New-Object psobject
Add-Member -InputObject $started -MemberType ScriptProperty -Name State -Value {{ {state} }}
Add-Member -InputObject $started -MemberType ScriptMethod -Name Refresh -Value {{ {refresh} }}
$task = New-Object psobject -Property @{{ State = 4 }}
Add-Member -InputObject $task -MemberType ScriptMethod -Name Run -Value {{ param($id) {run}
$started }}"
    );
    let mut text = text.to_owned();
    // A replacement that finds nothing would leave the real task and root in use: it is a failure.
    for (from, to) in [
        (
            format!("$root = {}", literal(ROOT)),
            format!("$root = {}", literal(&root.display().to_string())),
        ),
        (
            "$service = New-Object -ComObject 'Schedule.Service'".to_owned(),
            "$service = $null".to_owned(),
        ),
        ("$service.Connect()".to_owned(), String::new()),
        ("$task = $service.GetFolder('\\').GetTask('devvm-eb')".to_owned(), task),
    ] {
        assert!(text.contains(&from), "{from:?} is not in {text}");
        text = text.replace(&from, &to);
    }
    text
}

fn caller(root: &Path, run_body: &str, state: &str) -> Output {
    caller_with_refresh(root, run_body, "", state)
}

fn caller_with_refresh(root: &Path, run_body: &str, refresh: &str, state: &str) -> Output {
    run(&standin(root, run_body, refresh, state), "none", None)
}

/// The names in `root`: the requests, and the unpublished ones that end in `.tmp`.
fn requests(root: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(root) {
        Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

/// A runner that serves the request `$id` at once: it needs the request to exist, which is how the tests see that the
/// start was bound to it.
const SERVE: &str = "$r = Join-Path $root $id
if (-not [IO.Directory]::Exists($r)) { throw \"no request $id\" }
[IO.File]::WriteAllText(\"$r\\claim\", '')
[IO.File]::WriteAllText(\"$r\\stdout\", 'out')
[IO.File]::WriteAllText(\"$r\\stderr\", 'err')
[IO.File]::WriteAllText(\"$r\\exit\", '7')
[IO.File]::WriteAllText(\"$r\\done\", '')";

/// The instance the start returned is ready: it ended.
const ENDED: &str = "3";

#[skuld::test]
fn a_served_request_gives_its_output_and_its_status_and_is_told_it_was_read(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let output = caller(&root, SERVE, ENDED);

    assert_eq!(output.status.code(), Some(7), "{}", shown(&output));
    assert_eq!(output.stdout, b"out", "{}", shown(&output));
    assert_eq!(output.stderr, b"err", "{}", shown(&output));
    let [request] = requests(&root).try_into().expect("one request");
    let id = request.file_name().unwrap().to_str().unwrap();
    assert!(
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "the request is named by a guid: {id}"
    );
    // What the runner reads: the command, and the caller that ends it.
    assert_eq!(std::fs::read_to_string(request.join("script.ps1")).unwrap(), "inner");
    let caller_file = std::fs::read_to_string(request.join("caller")).unwrap();
    let words: Vec<&str> = caller_file.split(' ').collect();
    assert!(
        words.len() == 2 && words.iter().all(|word| word.parse::<u64>().is_ok()),
        "<pid> <start time in ticks>: {caller_file:?}"
    );
    // The caller's last act for a request a runner served: telling it everything was read.
    assert!(request.join("read").is_file());
}

#[skuld::test]
fn a_runner_that_died_without_a_result_is_126_and_the_request_is_removed(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let died = "[IO.File]::WriteAllText((Join-Path (Join-Path $root $id) 'claim'), '')";
    let output = caller(&root, died, ENDED);

    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(
        stderr(&output).contains("devvm: the elevated runner ended without a result"),
        "{}",
        shown(&output)
    );
    assert!(requests(&root).is_empty(), "{:?}", requests(&root));
}

#[skuld::test]
fn a_start_that_fails_is_126_and_the_request_is_withdrawn(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let output = caller(&root, "throw 'boom'", ENDED);

    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    let message = stderr(&output);
    assert!(
        message.contains("devvm: the elevated helper did not start: ") && message.contains("boom"),
        "{}",
        shown(&output)
    );
    assert!(requests(&root).is_empty(), "{:?}", requests(&root));
}

#[skuld::test]
fn a_helper_that_never_runs_the_command_is_126_and_the_request_is_withdrawn(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let output = caller(&root, "", ENDED);

    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    // The state of the instance, not the task's, which is running another request's instance.
    assert!(
        stderr(&output).contains("devvm: the elevated helper did not run the command (instance state: ready)"),
        "{}",
        shown(&output)
    );
    assert!(requests(&root).is_empty(), "{:?}", requests(&root));
}

/// The instance's refresh is an error: the one a finished instance may answer, which says it ended.
const NOT_RUNNING: &str =
    "throw [Runtime.InteropServices.COMException]::new('There is no running instance', [int]0x8004130B)";

#[skuld::test]
fn an_instance_that_answers_not_running_is_an_instance_that_ended(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let output = caller_with_refresh(&root, "", NOT_RUNNING, ENDED);

    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(
        stderr(&output).contains("devvm: the elevated helper did not run the command (instance state: not running)"),
        "{}",
        shown(&output)
    );
    assert!(requests(&root).is_empty(), "{:?}", requests(&root));
}

#[skuld::test]
fn an_instance_that_cannot_be_read_is_the_126_message_and_withdraws_the_request(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let output = caller_with_refresh(&root, "", "throw 'boom'", ENDED);

    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    let message = stderr(&output);
    assert!(
        message.contains("devvm: the elevated helper failed: ") && message.contains("boom"),
        "{}",
        shown(&output)
    );
    assert!(requests(&root).is_empty(), "{:?}", requests(&root));
}

/// A state body that answers `first` on its first read and `later` on every read after it: the instance changes while
/// the caller waits.
fn first_then(first: &str, later: &str) -> String {
    format!(
        "$global:reads = 1 + [int]$global:reads
if ($global:reads -eq 1) {{ {first} }} else {{ {later} }}"
    )
}

#[skuld::test]
fn an_instance_that_still_runs_is_waited_for_until_its_runner_answers(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    // The first look at the instance finds it running and no claim made: a caller that took that for ended would
    // withdraw the request. The runner answers at a later look.
    let output = caller(
        &root,
        "",
        &first_then(
            "4",
            &format!(
                "{SERVE}
4"
            ),
        ),
    );

    assert_eq!(output.status.code(), Some(7), "{}", shown(&output));
    assert_eq!(output.stdout, b"out", "{}", shown(&output));
}

#[skuld::test]
fn an_instance_in_a_state_that_is_neither_queued_nor_running_has_ended(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    // The first look reads `unknown` (0): neither queued nor running, so the wait ends. A caller that waited on it would
    // be served at the next look and answer 7.
    let output = caller(
        &root,
        "",
        &first_then(
            "0",
            &format!(
                "{SERVE}
4"
            ),
        ),
    );

    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(
        stderr(&output).contains("devvm: the elevated helper did not run the command (instance state: unknown)"),
        "{}",
        shown(&output)
    );
    assert!(requests(&root).is_empty(), "{:?}", requests(&root));
}

#[skuld::test]
fn an_instance_that_cannot_be_read_while_a_runner_claims_the_request_goes_on_with_the_runner(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    // The runner claims and answers between the caller's look at the claim and its look at the instance, which fails.
    let output = caller_with_refresh(
        &root,
        "",
        &format!(
            "{SERVE}
throw 'boom'"
        ),
        ENDED,
    );

    assert_eq!(output.status.code(), Some(7), "{}", shown(&output));
    assert_eq!(output.stdout, b"out", "{}", shown(&output));
}

/// The state of the instance reads as `$null`, which is how PowerShell answers a property getter that failed; `[int]$null`
/// would be `unknown`, which is not a state the instance has.
const UNREADABLE_STATE: &str = "$null";

#[skuld::test]
fn an_instance_whose_state_cannot_be_read_is_the_126_message_and_withdraws_the_request(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let output = caller(&root, "", UNREADABLE_STATE);

    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    let message = stderr(&output);
    assert!(
        message.contains(
            "devvm: the elevated helper failed: the state of the instance could not be read: the state read as empty"
        ),
        "{}",
        shown(&output)
    );
    assert!(requests(&root).is_empty(), "{:?}", requests(&root));
}

#[skuld::test]
fn an_instance_whose_state_cannot_be_read_while_a_runner_claims_the_request_goes_on_with_the_runner(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let output = caller(
        &root,
        "",
        &format!(
            "{SERVE}
{UNREADABLE_STATE}"
        ),
    );

    assert_eq!(output.status.code(), Some(7), "{}", shown(&output));
    assert_eq!(output.stdout, b"out", "{}", shown(&output));
}

#[skuld::test]
fn a_state_that_is_not_a_number_while_a_runner_claims_the_request_goes_on_with_the_runner(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let output = caller(
        &root,
        "",
        &format!(
            "{SERVE}
'not a number'"
        ),
    );

    assert_eq!(output.status.code(), Some(7), "{}", shown(&output));
    assert_eq!(output.stdout, b"out", "{}", shown(&output));
}

#[skuld::test]
fn a_start_that_returns_no_instance_is_a_helper_that_did_not_run_the_command(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let output = caller(&root, "return $null", ENDED);

    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(
        stderr(&output).contains("devvm: the elevated helper did not run the command (instance state: not started)"),
        "{}",
        shown(&output)
    );
    assert!(requests(&root).is_empty(), "{:?}", requests(&root));
}

#[skuld::test]
fn an_error_of_another_facility_with_the_code_of_not_running_is_a_failure(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let output = caller_with_refresh(
        &root,
        "",
        "throw [Runtime.InteropServices.COMException]::new('Not the task scheduler', [int]0x8007130B)",
        ENDED,
    );

    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(
        stderr(&output).contains("devvm: the elevated helper failed: "),
        "{}",
        shown(&output)
    );
    assert!(requests(&root).is_empty(), "{:?}", requests(&root));
}

#[skuld::test]
fn a_request_that_cannot_be_made_is_126_and_leaves_nothing_behind(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    // The root's parent is a file: nothing can be made under it. The status is not the one of a command that failed.
    let file = home.join("not-a-directory");
    std::fs::write(&file, "").unwrap();
    let output = caller(&file.join("eb"), SERVE, ENDED);

    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(
        stderr(&output).contains("devvm: the elevated helper could not make the request"),
        "{}",
        shown(&output)
    );
}

#[skuld::test]
fn a_request_that_is_gone_ends_the_wait_with_126_instead_of_waiting_for_ever(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    // Removed, and the start fails after it: no claim can be made, and none exists.
    let gone = "Remove-Item -LiteralPath (Join-Path $root $id) -Recurse -Force; throw 'boom'";
    let output = caller(&root, gone, ENDED);
    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(
        stderr(&output).contains("devvm: the elevated helper failed"),
        "{}",
        shown(&output)
    );

    // Removed after a start that went well: the wait sees it.
    let output = caller(
        &root,
        "Remove-Item -LiteralPath (Join-Path $root $id) -Recurse -Force",
        ENDED,
    );
    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(
        stderr(&output).contains("devvm: the elevated request was removed"),
        "{}",
        shown(&output)
    );
}

/// The runner plays the real one for the elevated form of a command: it starts the request's script as the runner does
/// (`pwsh -File`) and hands back what it wrote.
#[skuld::test]
fn an_elevated_powershell_command_gets_the_hint_through_the_request(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let runner = "$r = Join-Path $root $id
[IO.File]::WriteAllText(\"$r/claim\", '')
& pwsh -NoProfile -NonInteractive -File \"$r/script.ps1\" > \"$r/stdout\" 2> \"$r/stderr\"
[IO.File]::WriteAllText(\"$r/exit\", [string]$LASTEXITCODE)
[IO.File]::WriteAllText(\"$r/done\", '')";
    let mut command = Command::new(["Get-ChildItem"]);
    command.run_as = RunAs::Elevated;
    let script = standin_for(&wrap(&command).unwrap(), &root, runner, "", ENDED);
    let output = run(&script, "none", None);
    assert_eq!(output.status.code(), Some(127), "{}", shown(&output));
    assert!(stderr(&output).contains(&hint("Get-ChildItem")), "{}", shown(&output));
}

/// A `Run` that makes the request's files `created`, leaves the file `held` of it open to nobody else and ends in an
/// error, so the caller withdraws it. The next pause (`Start-Sleep`, replaced by a function) says on stderr which names
/// the root and the request then hold, and closes the file. A removal that stops after one try leaves the request; one
/// that tries again is let through by the pause.
fn held_until_the_pause(held: &str, created: &[&str]) -> String {
    let created = created
        .iter()
        .map(|name| format!("[IO.File]::WriteAllText((Join-Path $global:requestDir '{name}'), '')\n"))
        .collect::<String>();
    format!(
        "$global:requestDir = Join-Path $root $id
{created}$global:held = [IO.File]::Open((Join-Path $global:requestDir '{held}'), 'Open', 'Read', 'None')
function global:Start-Sleep {{ param($Milliseconds)
    if ($global:held) {{
        $root = Split-Path -Parent $global:requestDir
        [Console]::Error.WriteLine('PAUSED root=' + ((Get-ChildItem -LiteralPath $root -Force | ForEach-Object Name) -replace '^[0-9a-f]{{32}}', 'ID' -join ',') + ' request=' + ((Get-ChildItem -LiteralPath $global:requestDir -Force | ForEach-Object Name | Sort-Object) -join ','))
        $global:held.Dispose(); $global:held = $null
    }}
}}
throw 'boom'"
    )
}

#[skuld::test]
fn a_withdrawn_request_that_is_held_open_is_removed_once_it_is_released(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let output = caller(&root, &held_until_the_pause("script.ps1", &[]), ENDED);

    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(stderr(&output).contains("PAUSED"), "{}", shown(&output));
    assert!(requests(&root).is_empty(), "{:?}", requests(&root));
}

#[skuld::test]
fn a_withdrawn_request_is_removed_under_its_name_with_the_caller_and_the_claim_last(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let run = held_until_the_pause("stdout", &["done", "exit", "read", "stdout"]);
    let output = caller(&root, &run, ENDED);

    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    // The removal that failed on `stdout` had removed the files before it and kept `caller` and `claim`; and the request
    // was not renamed out of the requests' names.
    assert!(
        stderr(&output).contains("PAUSED root=ID request=caller,claim,stdout"),
        "{}",
        shown(&output)
    );
    assert!(requests(&root).is_empty(), "{:?}", requests(&root));
}

#[skuld::test]
fn a_withdrawn_request_that_cannot_be_removed_for_good_is_warned_about_and_left(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let denied = "function global:Remove-Item { throw [UnauthorizedAccessException]::new('denied') }
throw 'boom'";
    let output = caller(&root, denied, ENDED);

    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(
        stderr(&output).contains("devvm: warning: could not remove the withdrawn request: denied"),
        "{}",
        shown(&output)
    );
    assert_eq!(requests(&root).len(), 1, "{:?}", requests(&root));
}
