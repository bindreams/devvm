use super::total;
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
    let max = bytesize::ByteSize(u64::MAX);
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
    let max = bytesize::ByteSize(u64::MAX);
    assert_eq!(
        total(&dir(), &items),
        format!("2 files ({max}), 0 symlinks, 0 directories")
    );
}
