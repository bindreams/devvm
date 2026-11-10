use std::path::Path;

use super::{SCRATCH_DRAW_BYTES, scratch_name, scratch_owner, scratch_run_in};
use crate::backend::qemu::qmp::QmpEndpoint;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::roots;

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
    assert!(
        !scratch_owner(&leftover).exists(),
        "the name that was drawn again kept its .own file"
    );
}

#[skuld::test]
fn dropping_a_scratch_run_frees_its_name(#[fixture(unicode_home)] home: &Path) {
    drop(scratch_run_in(home, &mut draws([1])));

    let again = scratch_run_in(home, &mut draws([1]));

    assert_eq!(again.path(), home.join(scratch_name([1; SCRATCH_DRAW_BYTES])));
}

#[skuld::test]
fn the_socket_dir_predicate_reserves_a_scratch_runs_engine_layout() {
    let dir = Path::new("temp");
    let deepest = dir.join(scratch_name([0; SCRATCH_DRAW_BYTES])).join("devvm");
    let longest = dir
        .join(scratch_name([0; SCRATCH_DRAW_BYTES]))
        .join("devvm")
        .join("0000000000000000.sock");
    let bytes = longest.as_os_str().as_encoded_bytes().len();
    for max in [bytes - 1, bytes] {
        assert_eq!(roots::fits(dir, max), QmpEndpoint::fits(&deepest, max), "{max}");
    }
    assert!(roots::fits(dir, bytes));
    assert!(!roots::fits(dir, bytes - 1));
}
