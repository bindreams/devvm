use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use devvm::exit::{ExitStatus, ExitWidth};
use devvm::progress::{Event, Look, NullProgress, Outcome, Progress, Task, TaskId, Total};
use indicatif::{InMemoryTerm, ProgressDrawTarget, TermLike};

use crate::output::{Active, Panicked, Run};
use crate::tty::{Pace, TtyProgress};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

const PLAIN: Look = Look {
    colour: false,
    verbose: false,
};

/// What the screen does to start a fresh line, as the real one does on stderr.
fn newline(term: &InMemoryTerm) -> impl Fn() + Send + Sync + 'static + use<> {
    let term = term.clone();
    move || {
        term.write_line("").unwrap();
    }
}

fn screen(rows: u16, columns: u16, look: Look) -> (TtyProgress, InMemoryTerm) {
    let term = InMemoryTerm::new(rows, columns);
    let target = ProgressDrawTarget::term_like(Box::new(term.clone()));
    (
        TtyProgress::new(target, look, None, move || columns, newline(&term)),
        term,
    )
}

/// A screen whose width the test can change, as a person resizing the window does.
fn resizable(rows: u16, columns: u16) -> (TtyProgress, InMemoryTerm, Arc<AtomicU16>) {
    let term = InMemoryTerm::new(rows, columns);
    let target = ProgressDrawTarget::term_like(Box::new(term.clone()));
    let width = Arc::new(AtomicU16::new(columns));
    let read = width.clone();
    let tty = TtyProgress::new(
        target,
        PLAIN,
        None,
        move || read.load(Ordering::Relaxed),
        newline(&term),
    );
    (tty, term, width)
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
            "  building win.devvm.yaml [███░░░░░░░] 1/3",
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
            "  building win.devvm.yaml [███░░░░░░░] 1/3",
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
fn step_output_is_hidden_unless_verbose_and_its_controls_are_escaped() {
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
    loud.event(&Event::Output {
        line: "\u{1b}[32mgreen text\rredraw\0".to_owned(),
    });
    loud.debug("detail");
    assert_eq!(lines(&term), ["\\u{1b}[32mgreen text\\rredraw\\0", "detail"]);
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

#[skuld::test]
fn a_task_that_ends_before_its_child_leaves_the_child_drawn() {
    let (tty, term) = screen(6, 60, PLAIN);
    let [top, middle, bottom] = [(); 3].map(|()| id());
    begin(&tty, top, None, "top", Total::Unknown);
    begin(&tty, middle, Some(top), "middle", Total::Unknown);
    begin(&tty, bottom, Some(middle), "bottom", Total::Unknown);
    end(&tty, middle, Outcome::Finished);
    assert_eq!(lines(&term), ["- top", "-    └─ bottom"]);
    end(&tty, top, Outcome::Finished);
    assert_eq!(lines(&term), ["-    └─ bottom"]);
}

#[skuld::test]
fn a_task_of_unknown_size_shows_the_bytes_it_has_counted() {
    let (tty, term) = screen(4, 60, PLAIN);
    let task = id();
    begin(&tty, task, None, "downloading a.img", Total::Unknown);
    assert_eq!(lines(&term), ["- downloading a.img"]);
    advance(&tty, task, 5 * MIB);
    // The spinner's glyph turns with each report.
    let shown = lines(&term);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(shown[0].ends_with(" downloading a.img 5.0 MiB"), "{shown:?}");
}

/// A line that fills the terminal wraps on a ConPTY, so the room kept for a count follows the digits of its total.
#[skuld::test]
fn a_count_of_any_size_leaves_room_for_the_line() {
    let cases = [
        (Total::Items(1_000_000), 999_999, vec![20, 49, 50, 60, 100]),
        // Wider than any terminal below 60 columns can show with a bar.
        (Total::Items(u64::MAX), u64::MAX - 1, vec![60, 100]),
    ];
    for (total, position, widths) in cases {
        for columns in widths {
            let (tty, term) = screen(4, columns, PLAIN);
            let task = id();
            begin(&tty, task, None, &"x".repeat(200), total);
            advance(&tty, task, position);
            let shown: Vec<usize> = term.contents().lines().map(console::measure_text_width).collect();
            assert_eq!(shown.len(), 1, "{total:?}, {columns} columns: {:?}", term.contents());
            assert!(
                shown[0] < usize::from(columns),
                "{total:?}, {columns} columns: {}",
                shown[0]
            );
        }
    }
}

#[skuld::test]
fn a_resize_refits_the_line_at_the_next_report() {
    let (tty, term, width) = resizable(4, 100);
    let task = id();
    begin(&tty, task, None, &"x".repeat(200), Total::Bytes(GIB));
    advance(&tty, task, MIB);
    let before = console::measure_text_width(term.contents().trim_end());
    assert!(before > 60, "{before}");
    width.store(40, Ordering::Relaxed);
    advance(&tty, task, 2 * MIB);
    let after = console::measure_text_width(term.contents().trim_end());
    assert!(after < 40, "{after}: {:?}", term.contents());
    assert_eq!(term.contents().lines().count(), 1, "{:?}", term.contents());
}

// Ancestry ============================================================================================================

/// An ancestor that ended leaves a column that goes on while its parent has later children, and is blank after. The task
/// before it at that depth, an uncle, is not the ancestor.
#[skuld::test]
fn an_uncle_is_not_taken_for_an_ancestor_that_ended() {
    let (tty, term) = screen(8, 60, PLAIN);
    let [root, uncle, parent, child, later] = [(); 5].map(|()| id());
    begin(&tty, root, None, "R", Total::Unknown);
    begin(&tty, uncle, Some(root), "U", Total::Unknown);
    begin(&tty, parent, Some(root), "P", Total::Unknown);
    begin(&tty, child, Some(parent), "C", Total::Unknown);
    begin(&tty, later, Some(root), "L", Total::Unknown);
    assert_eq!(lines(&term), ["- R", "- ├─ U", "- ├─ P", "- │  └─ C", "- └─ L"]);
    end(&tty, parent, Outcome::Finished);
    assert_eq!(lines(&term), ["- R", "- ├─ U", "- │  └─ C", "- └─ L"]);
}

/// A child begun under a parent goes below all the parent's descendants, even one whose middle ancestor ended.
#[skuld::test]
fn a_child_goes_below_the_descendants_of_its_parent() {
    let (tty, term) = screen(8, 60, PLAIN);
    let [root, parent, middle, grandchild, sibling, other] = [(); 6].map(|()| id());
    begin(&tty, root, None, "R", Total::Unknown);
    begin(&tty, parent, Some(root), "P", Total::Unknown);
    begin(&tty, middle, Some(parent), "M", Total::Unknown);
    begin(&tty, grandchild, Some(middle), "G", Total::Unknown);
    end(&tty, middle, Outcome::Finished);
    begin(&tty, sibling, Some(parent), "S", Total::Unknown);
    begin(&tty, other, Some(root), "O", Total::Unknown);
    assert_eq!(lines(&term), ["- R", "- ├─ P", "- │  │  └─ G", "- │  └─ S", "- └─ O"]);
}

// Caching =============================================================================================================

/// What runs when a draw begins, and whether the draw then fails.
struct Trap {
    run: Box<dyn FnOnce() + Send>,
    fails: bool,
}

/// A `TermLike` that counts draws, and runs a closure at the next one.
#[derive(Clone)]
struct Probe {
    term: InMemoryTerm,
    draws: Arc<AtomicUsize>,
    trap: Arc<Mutex<Option<Trap>>>,
}

impl std::fmt::Debug for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Probe")
    }
}

impl Probe {
    fn new(term: &InMemoryTerm) -> Probe {
        Probe {
            term: term.clone(),
            draws: Arc::default(),
            trap: Arc::default(),
        }
    }

    /// Runs `trap` when the next draw begins, then panics, as a draw that fails does.
    fn trap_next_draw(&self, trap: impl FnOnce() + Send + 'static) {
        *self.trap.lock().unwrap() = Some(Trap {
            run: Box::new(trap),
            fails: true,
        });
    }

    /// Runs `hold` when the next draw begins, and draws on once it returns.
    fn hold_next_draw(&self, hold: impl FnOnce() + Send + 'static) {
        *self.trap.lock().unwrap() = Some(Trap {
            run: Box::new(hold),
            fails: false,
        });
    }

    fn tripped(&self) {
        let trap = self.trap.lock().unwrap().take();
        if let Some(trap) = trap {
            (trap.run)();
            if trap.fails {
                panic!("the terminal failed");
            }
        }
    }
}

impl TermLike for Probe {
    fn width(&self) -> u16 {
        self.tripped();
        self.term.width()
    }
    fn height(&self) -> u16 {
        self.term.height()
    }
    fn move_cursor_up(&self, n: usize) -> std::io::Result<()> {
        self.term.move_cursor_up(n)
    }
    fn move_cursor_down(&self, n: usize) -> std::io::Result<()> {
        self.term.move_cursor_down(n)
    }
    fn move_cursor_right(&self, n: usize) -> std::io::Result<()> {
        self.term.move_cursor_right(n)
    }
    fn move_cursor_left(&self, n: usize) -> std::io::Result<()> {
        self.term.move_cursor_left(n)
    }
    fn write_line(&self, s: &str) -> std::io::Result<()> {
        self.term.write_line(s)
    }
    fn write_str(&self, s: &str) -> std::io::Result<()> {
        self.term.write_str(s)
    }
    fn clear_line(&self) -> std::io::Result<()> {
        self.term.clear_line()
    }
    fn flush(&self) -> std::io::Result<()> {
        self.draws.fetch_add(1, Ordering::SeqCst);
        self.term.flush()
    }
}

fn probed(rows: u16, columns: u16) -> (TtyProgress, InMemoryTerm, Probe) {
    let term = InMemoryTerm::new(rows, columns);
    let probe = Probe::new(&term);
    let target = ProgressDrawTarget::term_like(Box::new(probe.clone()));
    let tty = TtyProgress::new(target, PLAIN, None, move || columns, newline(&term));
    (tty, term, probe)
}

/// A report that changes nothing about how the line is fitted sets the position and nothing else: a copy reports
/// at every MiB.
#[skuld::test]
fn a_report_that_changes_no_text_draws_once() {
    let (tty, _term, probe) = probed(4, 80);
    let [bytes, items] = [(); 2].map(|()| id());
    begin(&tty, bytes, None, "copying a.img", Total::Bytes(5 * GIB));
    begin(&tty, items, None, "step", Total::Items(100));
    advance(&tty, bytes, MIB);
    advance(&tty, items, 1);
    for (task, position) in [(bytes, 2 * MIB), (bytes, 3 * MIB), (items, 2), (items, 3)] {
        let before = probe.draws.load(Ordering::SeqCst);
        advance(&tty, task, position);
        let drew = probe.draws.load(Ordering::SeqCst) - before;
        assert!(drew <= 1, "{drew} draws for one report");
    }
}

#[skuld::test]
fn the_width_is_read_once_for_a_report() {
    let term = InMemoryTerm::new(4, 80);
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = reads.clone();
    let tty = TtyProgress::new(
        ProgressDrawTarget::term_like(Box::new(term.clone())),
        PLAIN,
        None,
        move || {
            counted.fetch_add(1, Ordering::SeqCst);
            80
        },
        newline(&term),
    );
    let [first, second] = [(); 2].map(|()| id());
    begin(&tty, first, None, "a", Total::Bytes(GIB));
    begin(&tty, second, None, "b", Total::Bytes(GIB));
    let before = reads.load(Ordering::SeqCst);
    advance(&tty, first, MIB);
    assert_eq!(reads.load(Ordering::SeqCst) - before, 1);
}

// Ending ==============================================================================================================

/// An `io::Write` onto the terminal, as stderr is: each complete line is written to it.
struct TermWriter {
    term: InMemoryTerm,
    pending: Vec<u8>,
}

impl TermWriter {
    fn new(term: &InMemoryTerm) -> TermWriter {
        TermWriter {
            term: term.clone(),
            pending: Vec::new(),
        }
    }
}

impl Write for TermWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        for &byte in bytes {
            if byte == b'\n' {
                let line = String::from_utf8(std::mem::take(&mut self.pending)).unwrap();
                self.term.write_line(&line)?;
            } else {
                self.pending.push(byte);
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A screen in a run, with the terminal it draws to and a probe of its draws.
fn run_with_screen(tick: Option<Duration>) -> (Arc<Run>, Arc<TtyProgress>, InMemoryTerm, Probe) {
    let term = InMemoryTerm::new(6, 60);
    let probe = Probe::new(&term);
    let tty = Arc::new(TtyProgress::new(
        ProgressDrawTarget::term_like(Box::new(probe.clone())),
        PLAIN,
        tick,
        || 60,
        newline(&term),
    ));
    let run = Arc::new(Run::new());
    run.begin(Active {
        log: None,
        tty: Some(tty.clone()),
    });
    (run, tty, term, probe)
}

/// What the panic hook does for `message`, writing to `term`: whether it was the ending.
fn hook(run: &Run, term: &InMemoryTerm, message: &str) -> bool {
    let panicked = run.on_panic(
        message.to_owned(),
        false,
        None,
        ExitWidth::Full32,
        &mut TermWriter::new(term),
    );
    matches!(panicked, Panicked::Ended { code: 125, .. })
}

/// The command's ending, with `bad` as its outcome, writing to `term`.
fn end_with_error(run: &Run, term: &InMemoryTerm) -> i32 {
    run.begin_ending().end(
        false,
        None,
        Err(devvm::Error::new(devvm::ErrorKind::Io, "bad")),
        ExitWidth::Full32,
        &mut TermWriter::new(term),
    )
}

#[skuld::test]
fn the_rows_drawn_follow_the_open_tasks() {
    let (tty, _term) = screen(6, 60, PLAIN);
    let rows = || tty.shared.rows.load(Ordering::SeqCst);
    let [first, second] = [(); 2].map(|()| id());
    assert_eq!(rows(), 0);
    begin(&tty, first, None, "one", Total::Unknown);
    begin(&tty, second, Some(first), "two", Total::Bytes(MIB));
    assert_eq!(rows(), 2);
    end(&tty, first, Outcome::Finished);
    assert_eq!(rows(), 1);
    end(&tty, second, Outcome::Finished);
    assert_eq!(rows(), 0);
}

/// The panic hook runs on the thread that is inside the draw, holding the screen's lock, with the cursor part way
/// through the bars: it cannot take the lock, and its newlines put the error line below them.
#[skuld::test]
fn after_a_panic_in_a_draw_the_error_line_is_the_last_line() {
    let (run, tty, term, probe) = run_with_screen(None);
    let [first, second] = [(); 2].map(|()| id());
    begin(&tty, first, None, "working", Total::Unknown);
    begin(&tty, second, None, "copying", Total::Bytes(GIB));
    let (hook_run, hook_term) = (run.clone(), term.clone());
    let was_the_ending = Arc::new(AtomicBool::new(false));
    let recorded = was_the_ending.clone();
    probe.trap_next_draw(move || recorded.store(hook(&hook_run, &hook_term, "the draw failed"), Ordering::SeqCst));
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| advance(&tty, second, 1)));
    assert!(unwound.is_err());
    assert!(was_the_ending.load(Ordering::SeqCst));
    let shown = lines(&term);
    assert_eq!(
        shown.last().map(String::as_str),
        Some("devvm-error: panic: the draw failed"),
        "{shown:?}"
    );
    // The screen is left alone afterwards: nothing hangs on the lock the panic poisoned, nothing more is drawn, and
    // dropping it does not touch what indicatif holds.
    begin(&tty, id(), None, "late", Total::Unknown);
    tty.warn("late note");
    assert_eq!(lines(&term), shown);
    drop(run);
    drop(tty);
}

#[skuld::test]
fn a_panic_with_nothing_drawn_writes_no_blank_line() {
    let (run, _tty, term, _probe) = run_with_screen(None);
    assert!(hook(&run, &term, "boom"));
    assert_eq!(lines(&term), ["devvm-error: panic: boom"]);
}

#[skuld::test]
fn a_panic_with_one_row_drawn_starts_one_line_below_it() {
    let (run, tty, term, _probe) = run_with_screen(None);
    begin(&tty, id(), None, "working", Total::Unknown);
    assert!(hook(&run, &term, "boom"));
    assert_eq!(lines(&term), ["- working", "devvm-error: panic: boom"]);
}

/// A spinner thread that panics inside a draw, while the command's ending waits for the screen: its hook cannot end
/// the run, so it stores the panic and unwinds, and the ending reports it.
#[skuld::test]
fn a_panic_on_the_spinner_thread_during_the_ending_is_reported_by_the_ending() {
    let (run, tty, term, probe) = run_with_screen(Some(Duration::from_millis(1)));
    begin(&tty, id(), None, "waiting", Total::Unknown);
    let ending = run.begin_ending();
    let (in_draw, seen) = mpsc::channel();
    let (hook_run, hook_term) = (run.clone(), term.clone());
    let was_the_ending = Arc::new(AtomicBool::new(true));
    let recorded = was_the_ending.clone();
    probe.trap_next_draw(move || {
        in_draw.send(()).unwrap();
        recorded.store(hook(&hook_run, &hook_term, "the spinner failed"), Ordering::SeqCst);
    });
    // The spinner thread is inside a draw, holding the screen's lock.
    seen.recv().unwrap();
    let code = ending.end(
        false,
        None,
        Ok(ExitStatus::Code(0)),
        ExitWidth::Full32,
        &mut TermWriter::new(&term),
    );
    assert_eq!(code, 125);
    assert!(!was_the_ending.load(Ordering::SeqCst), "the hook ended the run");
    let shown = lines(&term);
    assert_eq!(
        shown.last().map(String::as_str),
        Some("devvm-error: panic: the spinner failed"),
        "{shown:?}"
    );
    assert_eq!(
        shown.iter().filter(|line| line.starts_with("devvm-error")).count(),
        1,
        "{shown:?}"
    );
}

/// A draw in flight when the ending starts is over before the bars are cleared, and nothing drawn later can erase
/// what is written after.
#[skuld::test]
fn quiet_waits_for_a_draw_in_flight_and_what_is_written_after_it_stays() {
    let (run, tty, term, probe) = run_with_screen(None);
    let [spinner, bar] = [(); 2].map(|()| id());
    begin(&tty, spinner, None, "waiting", Total::Unknown);
    begin(&tty, bar, None, "copying", Total::Bytes(GIB));
    let (in_draw, seen) = mpsc::channel();
    let watched = tty.clone();
    probe.hold_next_draw(move || {
        in_draw.send(()).unwrap();
        // The ending has begun to stop the screen; its lock is what it waits for.
        while !watched.shared.is_quiet() {
            std::thread::yield_now();
        }
    });
    std::thread::scope(|scope| {
        scope.spawn(|| advance(&tty, bar, MIB));
        seen.recv().unwrap();
        end_with_error(&run, &term);
    });
    // A tick or a report that comes after draws nothing.
    tty.shared.tick();
    advance(&tty, bar, 2 * MIB);
    tty.warn("late");
    assert_eq!(lines(&term), ["devvm-error: io: bad"]);
}

#[skuld::test]
fn a_screen_a_draw_panicked_in_is_left_as_it_was_by_the_ending() {
    let (run, tty, term, probe) = run_with_screen(None);
    begin(&tty, id(), None, "working", Total::Unknown);
    probe.trap_next_draw(|| {});
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        begin(&tty, id(), None, "next", Total::Unknown);
    }));
    assert!(unwound.is_err());
    end_with_error(&run, &term);
    assert_eq!(lines(&term), ["- working", "devvm-error: io: bad"]);
    drop(run);
    drop(tty);
}

// Ticking =============================================================================================================

#[skuld::test]
fn the_spinners_thread_waits_for_a_spinner_and_for_a_stop() {
    let pace = Pace::default();
    std::thread::scope(|scope| {
        let turn = scope.spawn(|| pace.wait_turn(Duration::ZERO));
        pace.set_spinners(1);
        assert!(turn.join().unwrap(), "a spinner is to be turned");
    });
    pace.set_spinners(0);
    std::thread::scope(|scope| {
        let turn = scope.spawn(|| pace.wait_turn(Duration::ZERO));
        pace.stop();
        assert!(!turn.join().unwrap(), "stopped");
    });
    assert!(!pace.wait_turn(Duration::ZERO), "a stop stays");
}

#[skuld::test]
fn a_spinner_turns_by_itself_and_its_thread_ends_with_the_screen() {
    let term = InMemoryTerm::new(4, 60);
    let tty = TtyProgress::new(
        ProgressDrawTarget::term_like(Box::new(term.clone())),
        PLAIN,
        Some(Duration::from_millis(1)),
        || 60,
        newline(&term),
    );
    begin(&tty, id(), None, "waiting", Total::Unknown);
    let first = lines(&term);
    // Each tick turns the glyph; this waits for the first.
    while lines(&term) == first {
        std::thread::yield_now();
    }
    // Dropping the screen stops and joins the thread: a thread left behind would hang here or draw on.
    drop(tty);
}
