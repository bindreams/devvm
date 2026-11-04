use std::path::Path;

use super::{SCRATCH_DRAW_BYTES, scratch_name, scratch_run_in};
use crate::test_support::fixtures::unicode_home;

/// The draws in `names`, then none: a test that asks for more has drawn a name it should not have needed.
fn draws<const N: usize>(names: [u8; N]) -> impl FnMut() -> [u8; SCRATCH_DRAW_BYTES] {
    let mut names = names.into_iter();
    move || [names.next().expect("a name was drawn again"); SCRATCH_DRAW_BYTES]
}

#[skuld::test]
fn two_scratch_runs_that_draw_one_name_never_share_a_directory(#[fixture(unicode_home)] home: &Path) {
    let first = scratch_run_in(home, &mut draws([1]));
    let second = scratch_run_in(home, &mut draws([1, 2]));

    assert_eq!(first.path(), home.join(scratch_name([1; SCRATCH_DRAW_BYTES])));
    assert_eq!(second.path(), home.join(scratch_name([2; SCRATCH_DRAW_BYTES])));
}

#[skuld::test]
fn a_scratch_run_refuses_a_directory_that_exists_and_leaves_it(#[fixture(unicode_home)] home: &Path) {
    let leftover = home.join(scratch_name([1; SCRATCH_DRAW_BYTES]));
    std::fs::create_dir(&leftover).unwrap();
    std::fs::write(leftover.join("kept"), b"").unwrap();

    let run = scratch_run_in(home, &mut draws([1, 2]));
    drop(run);

    assert!(leftover.join("kept").exists());
}

#[skuld::test]
fn dropping_a_scratch_run_frees_its_name(#[fixture(unicode_home)] home: &Path) {
    drop(scratch_run_in(home, &mut draws([1])));

    let again = scratch_run_in(home, &mut draws([1]));

    assert_eq!(again.path(), home.join(scratch_name([1; SCRATCH_DRAW_BYTES])));
}
