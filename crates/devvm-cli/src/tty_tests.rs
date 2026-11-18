use devvm::progress::{Event, Look, NullProgress, Outcome, Progress, Task, TaskId, Total};
use indicatif::{InMemoryTerm, ProgressDrawTarget};

use crate::tty::TtyProgress;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

const PLAIN: Look = Look {
    colour: false,
    verbose: false,
};

fn screen(rows: u16, columns: u16, look: Look) -> (TtyProgress, InMemoryTerm) {
    let term = InMemoryTerm::new(rows, columns);
    let target = ProgressDrawTarget::term_like(Box::new(term.clone()));
    (TtyProgress::new(target, look, None, columns), term)
}

fn lines(term: &InMemoryTerm) -> Vec<String> {
    term.contents().lines().map(|line| line.trim_end().to_owned()).collect()
}

/// A task id that no screen has seen.
fn id() -> TaskId {
    Task::begin(&NullProgress, "id", Total::Unknown).id()
}

fn begin(tty: &TtyProgress, id: TaskId, parent: Option<TaskId>, label: &str, total: Total) {
    tty.event(&Event::Begin {
        id,
        parent,
        label: label.to_owned(),
        total,
    });
}

fn end(tty: &TtyProgress, id: TaskId, outcome: Outcome) {
    tty.event(&Event::End { id, outcome });
}

fn advance(tty: &TtyProgress, id: TaskId, position: u64) {
    tty.event(&Event::Advance { id, position });
}

#[skuld::test]
fn a_nested_tree_draws_with_glyphs() {
    let (tty, term) = screen(10, 60, PLAIN);
    let [building, hashing, step, download, launch] = [(); 5].map(|()| id());
    begin(&tty, building, None, "building win.devvm.yaml", Total::Items(3));
    advance(&tty, building, 1);
    begin(
        &tty,
        hashing,
        Some(building),
        "hashing windows.iso",
        Total::Bytes(5 * GIB),
    );
    advance(&tty, hashing, GIB / 2);
    begin(&tty, step, Some(building), "step 1 of 2", Total::Unknown);
    tty.event(&Event::Status {
        id: step,
        text: "apt-get update".to_owned(),
    });
    begin(
        &tty,
        download,
        Some(step),
        "downloading ubuntu.img",
        Total::Bytes(600 * MIB),
    );
    advance(&tty, download, 100 * MIB);
    begin(&tty, launch, None, "starting dev", Total::Unknown);
    tty.event(&Event::Status {
        id: launch,
        text: "waiting for SSH".to_owned(),
    });
    assert_eq!(
        lines(&term),
        [
            "  building win.devvm.ya… [███░░░░░░░] 1/3",
            "  ├─ hashing windows.iso [█░░░░░░░░░] 512.0 MiB/5.0 GiB",
            "- └─ step 1 of 2: apt-get update",
            "     └─ downloading ubu… [█░░░░░░░░░] 100.0 MiB/600.0 MiB",
            "- starting dev: waiting for SSH",
        ]
    );
    // The last child of a parent changes its glyph when its elder sibling ends.
    end(&tty, download, Outcome::Finished);
    end(&tty, hashing, Outcome::Finished);
    assert_eq!(
        lines(&term),
        [
            "  building win.devvm.ya… [███░░░░░░░] 1/3",
            "- └─ step 1 of 2: apt-get update",
            "- starting dev: waiting for SSH",
        ]
    );
}

#[skuld::test]
fn a_bytes_task_shows_human_sizes() {
    let (tty, term) = screen(5, 80, PLAIN);
    let task = id();
    begin(&tty, task, None, "hashing a.iso", Total::Bytes(5 * GIB));
    advance(&tty, task, 1536 * MIB);
    assert_eq!(lines(&term), ["  hashing a.iso [███░░░░░░░] 1.5 GiB/5.0 GiB"]);
}

#[skuld::test]
fn a_status_replaces_the_text_after_the_label() {
    let (tty, term) = screen(5, 40, PLAIN);
    let task = id();
    begin(&tty, task, None, "Windows base", Total::Unknown);
    assert_eq!(lines(&term), ["- Windows base"]);
    for status in ["launch 1", "launch 2: first-logon"] {
        tty.event(&Event::Status {
            id: task,
            text: status.to_owned(),
        });
        assert_eq!(lines(&term), [format!("- Windows base: {status}")]);
    }
}

#[skuld::test]
fn an_ended_task_leaves_no_line() {
    let (tty, term) = screen(5, 60, PLAIN);
    let (first, second) = (id(), id());
    begin(&tty, first, None, "one", Total::Unknown);
    begin(&tty, second, None, "two", Total::Unknown);
    end(&tty, first, Outcome::Finished);
    assert_eq!(lines(&term), ["- two"]);
    end(&tty, second, Outcome::Finished);
    assert_eq!(lines(&term), Vec::<String>::new());
}

#[skuld::test]
fn an_abandoned_nested_task_leaves_the_screen_empty() {
    let (tty, term) = screen(5, 60, PLAIN);
    let progress: &dyn Progress = &tty;
    {
        let _outer = progress.task("building", Total::Items(2));
        let _middle = progress.task("step 1 of 1", Total::Unknown);
        let _inner = progress.task("downloading a.img", Total::Bytes(MIB));
        assert_eq!(lines(&term).len(), 3, "{:?}", lines(&term));
        // An error return drops the three, innermost first.
    }
    assert_eq!(lines(&term), Vec::<String>::new());
}

#[skuld::test]
fn a_warning_prints_above_the_bars_and_stays() {
    let (tty, term) = screen(6, 60, PLAIN);
    let task = id();
    begin(&tty, task, None, "step 1 of 1", Total::Unknown);
    tty.warn("cannot remove x");
    tty.message("info line");
    assert_eq!(lines(&term), ["warning: cannot remove x", "info line", "- step 1 of 1"]);
    end(&tty, task, Outcome::Abandoned);
    assert_eq!(lines(&term), ["warning: cannot remove x", "info line"]);
}

#[skuld::test]
fn a_warning_prefix_is_yellow_only_when_asked() {
    let (tty, term) = screen(
        4,
        60,
        Look {
            colour: true,
            verbose: false,
        },
    );
    tty.warn("careful");
    assert!(
        term.contents_formatted().windows(5).any(|w| w == b"\x1b[33m"),
        "no yellow"
    );
    let (plain, term) = screen(4, 60, PLAIN);
    plain.warn("careful");
    assert!(!term.contents_formatted().windows(5).any(|w| w == b"\x1b[33m"));
}

#[skuld::test]
fn step_output_is_hidden_unless_verbose_and_stripped_of_ansi() {
    let (quiet, term) = screen(5, 60, PLAIN);
    quiet.output("hidden");
    quiet.debug("also hidden");
    assert_eq!(lines(&term), Vec::<String>::new());

    let (loud, term) = screen(
        5,
        60,
        Look {
            colour: false,
            verbose: true,
        },
    );
    loud.output("\u{1b}[32mgreen\u{1b}[0m text\rredraw\0");
    loud.debug("detail");
    assert_eq!(lines(&term), ["green text\\rredraw\\0", "detail"]);
}

#[skuld::test]
fn a_begin_whose_parent_was_never_seen_is_drawn_as_a_root() {
    let (tty, term) = screen(5, 60, PLAIN);
    let (unseen, child) = (id(), id());
    begin(&tty, child, Some(unseen), "orphan", Total::Unknown);
    assert_eq!(lines(&term), ["- orphan"]);
}

#[skuld::test]
fn a_label_wider_than_the_terminal_does_not_leave_a_stale_line() {
    let (tty, term) = screen(5, 20, PLAIN);
    let task = id();
    begin(
        &tty,
        task,
        None,
        "hashing 世界世界世界世界世界世界.iso",
        Total::Bytes(GIB),
    );
    advance(&tty, task, MIB);
    assert_eq!(lines(&term).len(), 1, "{:?}", lines(&term));
    end(&tty, task, Outcome::Finished);
    assert_eq!(lines(&term), Vec::<String>::new());
}

/// A line that fills the terminal wraps on a ConPTY, where the redraw clears one row of its two and leaves the bar on
/// screen after the task ends.
#[skuld::test]
fn no_line_fills_the_width_of_the_terminal() {
    for columns in [20, 30, 40, 49, 50, 60, 100] {
        let (tty, term) = screen(8, columns, PLAIN);
        let [bytes, items, unknown] = [(); 3].map(|()| id());
        let long = "x".repeat(200);
        begin(&tty, bytes, None, &long, Total::Bytes(5 * GIB));
        advance(&tty, bytes, 4 * GIB);
        begin(&tty, items, Some(bytes), &long, Total::Items(100));
        advance(&tty, items, 50);
        begin(&tty, unknown, Some(items), &long, Total::Unknown);
        for line in term.contents().lines() {
            let width = console::measure_text_width(line.trim_end());
            assert!(width < usize::from(columns), "{columns} columns: {width} in {line:?}");
        }
        assert_eq!(
            term.contents().lines().count(),
            3,
            "{columns} columns: {:?}",
            term.contents()
        );
    }
}
