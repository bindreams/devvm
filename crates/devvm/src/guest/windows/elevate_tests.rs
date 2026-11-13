//! Golden tests of the caller script. The behaviour of its files and of the runner is measured on a guest.

use super::{CALLER, CLAIM, READ, caller_script, literal};

fn script() -> String {
    caller_script("inner")
}

fn at(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is not in {text}"))
}

#[skuld::test]
fn the_task_is_looked_up_before_the_request_is_published_and_started_after() {
    let text = script();
    let lookup = at(&text, ".GetTask('devvm-eb')");
    let built = at(&text, "CreateDirectory($building)");
    let published = at(&text, "[IO.Directory]::Move($building, $dir)");
    let started = at(&text, "$task.Run(");
    assert!(lookup < built && built < published && published < started, "{text}");
}

#[skuld::test]
fn a_task_that_cannot_be_found_ends_with_126_before_anything_is_published() {
    let text = script();
    let published = at(&text, "[IO.Directory]::Move($building, $dir)");
    let refusal = at(&text, "the elevated helper is not available");
    assert!(refusal < published, "{text}");
    assert!(text[refusal..published].contains("exit 126"), "{text}");
}

#[skuld::test]
fn a_start_that_fails_withdraws_the_request_with_a_claim_of_the_callers_own() {
    let text = script();
    let started = at(&text, "$task.Run(");
    let wait = at(&text, "while (-not $message)");
    let handler = &text[started..wait];
    // The same `CreateNew` that keeps a runner from taking the request; a claim that is not ours is a runner's, and the
    // wait goes on.
    assert!(handler.contains("'CreateNew', 'Write', 'None'"), "{handler}");
    assert!(handler.contains("$abandoned = $true"), "{handler}");
    assert!(
        handler.contains("catch [IO.IOException] { $message = $null }"),
        "{handler}"
    );
    assert!(handler.contains("the elevated helper did not start"), "{handler}");
}

#[skuld::test]
fn the_cleanup_runs_in_a_finally_and_a_request_a_runner_may_hold_is_signalled_not_removed() {
    let text = script();
    let finally = at(&text, "} finally {");
    // The only removal is the abandoned request's, in the `finally`; the read request is only told it was read.
    assert_eq!(text.matches("Remove-Item").count(), 1, "{text}");
    let removal = at(&text, "Remove-Item");
    let signal = at(&text, &format!("'{READ}'"));
    let abandoned = at(&text[finally..], "if ($abandoned)") + finally;
    assert!(abandoned < removal && removal < signal, "{text}");
    assert!(finally < abandoned, "{text}");
    // Both come before the script's own end.
    assert!(signal < at(&text, "exit $code"), "{text}");
    // The streams are released first.
    assert!(at(&text, "$stream.Dispose()") < abandoned, "{text}");
}

#[skuld::test]
fn a_runner_that_died_or_a_helper_that_did_not_run_leaves_the_request_to_the_caller() {
    let text = script();
    for message in [
        "the elevated runner ended without a result",
        "the elevated helper did not run the command",
    ] {
        let place = at(&text, message);
        assert!(
            text[place..]
                .lines()
                .take(3)
                .any(|line| line.contains("$abandoned = $true")),
            "{message}: {text}"
        );
    }
    // The error and its status come after the cleanup.
    assert!(at(&text, "} finally {") < text.rfind("exit 126 }").unwrap(), "{text}");
}

#[skuld::test]
fn the_request_names_its_caller_by_process_id_and_start_time_before_it_is_published() {
    let text = script();
    let caller = at(
        &text,
        &format!("{CALLER}\", \"$($self.Id) $($self.StartTime.ToUniversalTime().Ticks)\""),
    );
    assert!(caller < at(&text, "[IO.Directory]::Move($building, $dir)"), "{text}");
}

#[skuld::test]
fn the_requests_files_are_the_ones_the_runner_contract_names() {
    let text = script();
    for name in [CLAIM, "done", "exit", "stdout", "stderr", READ] {
        assert!(
            text.contains(&literal(name)) || text.contains(&format!("'{name}'")),
            "{name}: {text}"
        );
    }
}
