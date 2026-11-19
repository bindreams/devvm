use super::{byte_total, total};
use crate::access::sftp::plan::{Item, Kind};

fn file(size: u64) -> Item {
    Item {
        rel: vec!["f".to_owned()],
        kind: Kind::File {
            executable: false,
            mtime: None,
            size,
        },
    }
}

fn dir() -> Kind {
    Kind::Dir { mtime: None }
}

#[skuld::test]
fn the_total_counts_what_the_walk_found() {
    let items = [file(1024), file(1024)];
    assert_eq!(total(&dir(), &items), "2 files (2.0 KiB), 0 symlinks, 0 directories");
}

#[skuld::test]
fn sizes_that_sum_past_u64_are_announced_as_more_than_the_largest_total() {
    // A guest reports these sizes; none of them is checked against the others.
    let items = [file(1 << 63), file(1 << 63), file(1)];
    let max = crate::progress::human_bytes(u64::MAX);
    assert_eq!(
        total(&dir(), &items),
        format!("3 files (more than {max}), 0 symlinks, 0 directories")
    );
    assert_eq!(
        total(&file(u64::MAX).kind, &[file(u64::MAX)]),
        format!("2 files (more than {max}), 0 symlinks, 0 directories")
    );
}

#[skuld::test]
fn a_sum_that_ends_exactly_at_u64_max_is_not_an_overflow() {
    let items = [file(u64::MAX - 1), file(1)];
    let max = crate::progress::human_bytes(u64::MAX);
    assert_eq!(
        total(&dir(), &items),
        format!("2 files ({max}), 0 symlinks, 0 directories")
    );
}

#[skuld::test]
fn the_task_total_is_the_sum_of_the_file_sizes() {
    let items = [file(1024), file(1), dir_item()];
    assert_eq!(byte_total(&dir(), &items), 1025);
    assert_eq!(byte_total(&file(7).kind, &[]), 7);
    assert_eq!(byte_total(&dir(), &[]), 0);
}

#[skuld::test]
fn a_copy_past_u64_max_total_still_reports() {
    // A guest reports these sizes; the task's total is the largest there is, not a wrapped or refused one.
    let items = [file(1 << 63), file(1 << 63), file(1)];
    assert_eq!(byte_total(&dir(), &items), u64::MAX);
}

fn dir_item() -> Item {
    Item {
        rel: vec!["d".to_owned()],
        kind: dir(),
    }
}
