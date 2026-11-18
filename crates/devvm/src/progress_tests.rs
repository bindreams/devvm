use std::sync::Arc;

use crate::progress::{Event, Fanout, Level, Outcome, Progress, Task, TaskId, Total, human_bytes};
use crate::test_support::fixtures::MemoryProgress;

const MIB: u64 = 1 << 20;

fn begin(events: &[Event], label: &str) -> (TaskId, Option<TaskId>) {
    events
        .iter()
        .find_map(|event| match event {
            Event::Begin {
                id, parent, label: l, ..
            } if l == label => Some((*id, *parent)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no Begin for {label:?} in {events:?}"))
}

fn ends(events: &[Event]) -> Vec<(TaskId, Outcome)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::End { id, outcome } => Some((*id, *outcome)),
            _ => None,
        })
        .collect()
}

fn advances(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Advance { position, .. } => Some(*position),
            _ => None,
        })
        .collect()
}

#[skuld::test]
fn a_task_begun_inside_another_is_its_child() {
    let sink = MemoryProgress::default();
    let progress: &dyn Progress = &sink;
    {
        let outer = progress.task("outer", Total::Unknown);
        let inner = progress.task("inner", Total::Items(3));
        inner.finish();
        outer.finish();
    }
    let events = sink.events();
    let (outer, outer_parent) = begin(&events, "outer");
    let (_, inner_parent) = begin(&events, "inner");
    assert_eq!(outer_parent, None);
    assert_eq!(inner_parent, Some(outer));
}

#[skuld::test]
fn siblings_and_a_second_thread_do_not_nest() {
    let sink = MemoryProgress::default();
    let progress: &dyn Progress = &sink;
    {
        let outer = progress.task("outer", Total::Unknown);
        progress.task("first", Total::Unknown).finish();
        progress.task("second", Total::Unknown).finish();
        std::thread::scope(|scope| {
            scope.spawn(|| progress.task("elsewhere", Total::Unknown).finish());
        });
        outer.finish();
    }
    let events = sink.events();
    let (outer, _) = begin(&events, "outer");
    assert_eq!(begin(&events, "first").1, Some(outer));
    assert_eq!(begin(&events, "second").1, Some(outer));
    assert_eq!(begin(&events, "elsewhere").1, None);
}

#[skuld::test]
fn dropping_a_task_without_finish_ends_it_abandoned() {
    let sink = MemoryProgress::default();
    let progress: &dyn Progress = &sink;
    let task = progress.task("download", Total::Bytes(10));
    let id = task.id();
    drop(task);
    assert_eq!(ends(&sink.events()), vec![(id, Outcome::Abandoned)]);
}

#[skuld::test]
fn an_error_return_ends_nested_tasks_innermost_first() {
    fn fails(progress: &dyn Progress) -> Result<(), ()> {
        let _outer = progress.task("outer", Total::Unknown);
        let _middle = progress.task("middle", Total::Unknown);
        let _inner = progress.task("inner", Total::Unknown);
        Err(())
    }
    let sink = MemoryProgress::default();
    fails(&sink).unwrap_err();
    let events = sink.events();
    let ids: Vec<TaskId> = ["inner", "middle", "outer"]
        .map(|label| begin(&events, label).0)
        .to_vec();
    assert_eq!(
        ends(&events),
        ids.into_iter().map(|id| (id, Outcome::Abandoned)).collect::<Vec<_>>()
    );
}

#[skuld::test]
fn finish_ends_finished() {
    let sink = MemoryProgress::default();
    let progress: &dyn Progress = &sink;
    let task = progress.task("hash", Total::Unknown);
    let id = task.id();
    task.finish();
    assert_eq!(ends(&sink.events()), vec![(id, Outcome::Finished)]);
}

#[skuld::test]
fn a_task_that_ended_is_not_the_parent_of_the_next() {
    let sink = MemoryProgress::default();
    let progress: &dyn Progress = &sink;
    progress.task("one", Total::Unknown).finish();
    progress.task("two", Total::Unknown).finish();
    assert_eq!(begin(&sink.events(), "two").1, None);
}

#[skuld::test]
fn bytes_are_reported_per_mib_after_the_first_position_and_never_go_back() {
    let sink = MemoryProgress::default();
    let progress: &dyn Progress = &sink;
    let task = progress.task("download", Total::Bytes(10 * MIB));
    for position in [
        100,
        MIB - 1,
        MIB,
        MIB + 5,
        2 * MIB - 1,
        3 * MIB,
        2 * MIB,
        3 * MIB,
        3 * MIB + 1,
    ] {
        task.set(position);
    }
    // The total is reported even when it is not a whole MiB past the last report.
    task.set(10 * MIB);
    task.finish();
    // The first position is reported whatever it is: a resumed download starts somewhere.
    assert_eq!(advances(&sink.events()), vec![100, MIB, 3 * MIB, 10 * MIB]);
}

#[skuld::test]
fn items_are_reported_each_time_they_grow() {
    let sink = MemoryProgress::default();
    let progress: &dyn Progress = &sink;
    let items = progress.task("steps", Total::Items(3));
    for position in [1, 1, 2, 1, 3] {
        items.set(position);
    }
    items.finish();
    assert_eq!(advances(&sink.events()), vec![1, 2, 3]);
}

#[skuld::test]
fn status_names_its_task() {
    let sink = MemoryProgress::default();
    let progress: &dyn Progress = &sink;
    let task = progress.task("Windows base", Total::Unknown);
    task.status("launch 1");
    let id = task.id();
    task.finish();
    assert!(
        sink.events().contains(&Event::Status {
            id,
            text: "launch 1".to_owned()
        }),
        "{:?}",
        sink.events()
    );
}

#[skuld::test]
fn the_note_helpers_carry_their_level() {
    let sink = MemoryProgress::default();
    sink.message("a");
    sink.warn("b");
    sink.debug("c");
    sink.output("d");
    let note = |level, text: &str| Event::Note {
        level,
        text: text.to_owned(),
    };
    assert_eq!(
        sink.events(),
        vec![
            note(Level::Info, "a"),
            note(Level::Warn, "b"),
            note(Level::Debug, "c"),
            Event::Output { line: "d".to_owned() },
        ]
    );
    // Existing readers of `messages` see every note and output line, whatever its level.
    assert_eq!(sink.messages(), ["a", "b", "c", "d"]);
}

#[skuld::test]
fn fanout_forwards_each_event_to_every_sink_in_order() {
    let first = Arc::new(MemoryProgress::default());
    let second = Arc::new(MemoryProgress::default());
    let fanout = Fanout::new(vec![first.clone(), second.clone()]);
    let progress: &dyn Progress = &fanout;
    progress.message("a");
    Task::begin(progress, "t", Total::Items(1)).finish();
    progress.warn("b");
    assert_eq!(first.events().len(), 4);
    assert_eq!(first.events(), second.events());
}

#[skuld::test]
fn human_bytes_uses_iec_units() {
    assert_eq!(human_bytes(1_610_612_736), "1.5 GiB");
    assert_eq!(human_bytes(512 * MIB), "512.0 MiB");
    assert_eq!(human_bytes(0), "0 B");
}
