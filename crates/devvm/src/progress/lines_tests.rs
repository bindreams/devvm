use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::progress::lines::{LineProgress, Look};
use crate::progress::{Progress, Task, Total};
use crate::test_support::fixtures::MemoryProgress;

const MIB: u64 = 1 << 20;

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buffer {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

const PLAIN: Look = Look {
    colour: false,
    verbose: false,
};
const VERBOSE: Look = Look {
    colour: false,
    verbose: true,
};
const COLOUR: Look = Look {
    colour: true,
    verbose: false,
};

fn sink(look: Look) -> (LineProgress, Buffer) {
    let buffer = Buffer::default();
    (LineProgress::to(Box::new(buffer.clone()), look), buffer)
}

#[skuld::test]
fn a_download_prints_start_quarters_and_done() {
    let (progress, out) = sink(PLAIN);
    let progress: &dyn Progress = &progress;
    let task = progress.task("downloading a.img", Total::Bytes(400 * MIB));
    for mib in 1..=400 {
        task.set(mib * MIB);
    }
    task.finish();
    assert_eq!(
        out.lines(),
        [
            "downloading a.img (400.0 MiB)",
            "downloading a.img: 25%",
            "downloading a.img: 50%",
            "downloading a.img: 75%",
            "downloading a.img: done",
        ]
    );
}

#[skuld::test]
fn a_jump_past_several_quarters_prints_the_last_one_once() {
    let (progress, out) = sink(PLAIN);
    let progress: &dyn Progress = &progress;
    let task = progress.task("hashing a.iso", Total::Bytes(100 * MIB));
    task.set(80 * MIB);
    task.set(90 * MIB);
    task.finish();
    assert_eq!(
        out.lines(),
        ["hashing a.iso (100.0 MiB)", "hashing a.iso: 75%", "hashing a.iso: done"]
    );
}

#[skuld::test]
fn an_unsized_task_prints_its_label_and_each_status() {
    let (progress, out) = sink(PLAIN);
    let progress: &dyn Progress = &progress;
    let task = progress.task("Windows base", Total::Unknown);
    task.status("launch 1");
    task.status("launch 2: first-logon");
    task.finish();
    assert_eq!(
        out.lines(),
        [
            "Windows base",
            "Windows base: launch 1",
            "Windows base: launch 2: first-logon"
        ]
    );
}

#[skuld::test]
fn an_item_count_prints_no_advances_and_done_at_its_end() {
    let (progress, out) = sink(PLAIN);
    let progress: &dyn Progress = &progress;
    let task = progress.task("building x", Total::Items(3));
    task.set(1);
    task.set(2);
    task.finish();
    assert_eq!(out.lines(), ["building x", "building x: done"]);
}

#[skuld::test]
fn children_are_indented_two_spaces_per_level() {
    let (progress, out) = sink(PLAIN);
    let progress: &dyn Progress = &progress;
    let root = progress.task("building", Total::Items(2));
    let child = progress.task("step 1 of 2", Total::Unknown);
    let grandchild = progress.task("downloading b.img", Total::Bytes(MIB));
    grandchild.set(MIB);
    grandchild.finish();
    child.status("running");
    child.finish();
    root.finish();
    assert_eq!(
        out.lines(),
        [
            "building",
            "  step 1 of 2",
            "    downloading b.img (1.0 MiB)",
            "    downloading b.img: done",
            "  step 1 of 2: running",
            "building: done",
        ]
    );
}

#[skuld::test]
fn an_abandoned_item_count_prints_nothing_after_its_label() {
    let (progress, out) = sink(PLAIN);
    let progress: &dyn Progress = &progress;
    let task = progress.task("building x", Total::Items(3));
    task.set(1);
    drop(task);
    assert_eq!(out.lines(), ["building x"]);
}

#[skuld::test]
fn an_abandoned_task_prints_nothing() {
    let (progress, out) = sink(PLAIN);
    let progress: &dyn Progress = &progress;
    let task = progress.task("downloading a.img", Total::Bytes(10 * MIB));
    task.set(MIB);
    drop(task);
    assert_eq!(out.lines(), ["downloading a.img (10.0 MiB)"]);
}

#[skuld::test]
fn an_ended_task_is_not_the_parent_of_the_next() {
    let (progress, out) = sink(PLAIN);
    let progress: &dyn Progress = &progress;
    progress.task("one", Total::Unknown).finish();
    progress.task("two", Total::Unknown).finish();
    assert_eq!(out.lines(), ["one", "two"]);
}

#[skuld::test]
fn warn_gets_a_prefix_and_colour_only_when_asked() {
    let (plain, out) = sink(PLAIN);
    plain.warn("cannot x");
    plain.message("hello");
    assert_eq!(out.lines(), ["warning: cannot x", "hello"]);

    let (coloured, out) = sink(COLOUR);
    coloured.warn("cannot x");
    coloured.message("hello");
    assert_eq!(out.lines(), ["\u{1b}[33mwarning:\u{1b}[0m cannot x", "hello"]);
}

#[skuld::test]
fn done_is_green_when_colour_is_asked() {
    let (progress, out) = sink(COLOUR);
    let progress: &dyn Progress = &progress;
    let task = progress.task("hashing a", Total::Bytes(MIB));
    task.set(MIB);
    task.finish();
    assert_eq!(out.lines().last().unwrap(), "hashing a: \u{1b}[32mdone\u{1b}[0m");
}

#[skuld::test]
fn debug_and_output_print_only_when_verbose() {
    let (quiet, out) = sink(PLAIN);
    quiet.debug("a fact");
    quiet.output("a step line");
    assert_eq!(out.lines(), Vec::<String>::new());

    let (verbose, out) = sink(VERBOSE);
    verbose.debug("a fact");
    verbose.output("a step line");
    assert_eq!(out.lines(), ["a fact", "a step line"]);
}

#[skuld::test]
fn controls_in_a_note_are_escaped_so_a_line_stays_one_line() {
    let (progress, out) = sink(VERBOSE);
    progress.message("a\rb\u{1b}[31mc\0d\ne\tf");
    progress.output("C:\\Users\\x\r");
    assert_eq!(out.lines(), ["a\\rb\\u{1b}[31mc\\0d\\ne\\tf", "C:\\Users\\x\\r"]);
}

#[skuld::test]
fn controls_in_a_label_and_a_status_are_escaped() {
    let (progress, out) = sink(PLAIN);
    let progress: &dyn Progress = &progress;
    let task = progress.task("a\nb", Total::Unknown);
    task.status("c\rd");
    task.finish();
    assert_eq!(out.lines(), ["a\\nb", "a\\nb: c\\rd"]);
}

#[skuld::test]
fn a_begin_whose_parent_was_never_seen_is_printed_as_a_root() {
    let (progress, out) = sink(PLAIN);
    let progress: &dyn Progress = &progress;
    // The parent begins on a sink that is not `progress`, so the line renderer never sees it.
    let elsewhere = MemoryProgress::default();
    let parent = Task::begin(&elsewhere, "parent", Total::Unknown);
    let child = progress.task("child", Total::Unknown);
    child.status("x");
    child.finish();
    parent.finish();
    assert_eq!(out.lines(), ["child", "child: x"]);
}

#[skuld::test]
fn two_threads_never_interleave_inside_a_line() {
    let (progress, out) = sink(PLAIN);
    let long_a = "a".repeat(4000);
    let long_b = "b".repeat(4000);
    std::thread::scope(|scope| {
        for text in [&long_a, &long_b] {
            let progress = &progress;
            scope.spawn(move || {
                for _ in 0..200 {
                    progress.message(text);
                }
            });
        }
    });
    let lines = out.lines();
    assert_eq!(lines.len(), 400);
    assert!(lines.iter().all(|line| *line == long_a || *line == long_b));
}

#[skuld::test]
fn colour_escapes_in_step_output_are_dropped() {
    let (progress, out) = sink(VERBOSE);
    progress.output("\u{1b}[32mgreen\u{1b}[0m text\u{1b}[2K");
    assert_eq!(out.lines(), ["green text"]);
}
