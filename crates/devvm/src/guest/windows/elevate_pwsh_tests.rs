//! The caller script of an elevated command in a real PowerShell 7 (the `pwsh_wrap` group), against a stand-in for the
//! scheduled task and its runner. The runner's side of the contract is plain files, so a few lines of PowerShell play
//! it; the real task and runner are measured on a guest. The file names below are literals on purpose: they are the
//! runner's contract, and renaming a constant must not rename them here.

use std::path::{Path, PathBuf};
use std::process::Output;

use super::{ROOT, caller_script, literal};
use crate::guest::windows::wrap::wrap_pwsh_tests::{run, shown};
use crate::test_groups::{Group, pwsh_wrap};
use crate::test_support::fixtures::unicode_home;

/// `caller_script` with the scheduled task and the requests' root replaced: `run` is the body of the task's `Run` (it
/// has the request's id as `$id` and the root as `$root`) and `instances` the body of `GetInstances`.
fn standin(root: &Path, run: &str, instances: &str) -> String {
    let text = caller_script("inner");
    let task = format!(
        "$task = New-Object psobject -Property @{{ State = 4 }}
Add-Member -InputObject $task -MemberType ScriptMethod -Name Run -Value {{ param($id) {run} }}
Add-Member -InputObject $task -MemberType ScriptMethod -Name GetInstances -Value {{ param($flags) {instances} }}"
    );
    let mut text = text;
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

fn caller(root: &Path, run_body: &str, instances: &str) -> Output {
    run(&standin(root, run_body, instances), "none", None)
}

/// The names in `root`: the requests, and the unpublished ones that end in `.tmp`.
fn requests(root: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(root) {
        Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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

/// No instance of the task is running.
const NONE: &str = ",@()";

#[skuld::test]
fn a_served_request_gives_its_output_and_its_status_and_is_told_it_was_read(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pwsh_wrap)] _group: &Group,
) {
    let root = home.join("eb");
    let output = caller(&root, SERVE, NONE);

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
    let output = caller(&root, died, NONE);

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
    let output = caller(&root, "throw 'boom'", NONE);

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
    let output = caller(&root, "", NONE);

    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(
        stderr(&output).contains("devvm: the elevated helper did not run the command"),
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
    let output = caller(&file.join("eb"), SERVE, NONE);

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
    let output = caller(&root, gone, NONE);
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
        NONE,
    );
    assert_eq!(output.status.code(), Some(126), "{}", shown(&output));
    assert!(
        stderr(&output).contains("devvm: the elevated request was removed"),
        "{}",
        shown(&output)
    );
}
