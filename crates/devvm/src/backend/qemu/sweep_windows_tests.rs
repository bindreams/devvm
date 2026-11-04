use std::path::Path;

use super::sweep_tests::{STEM_A, run_with_a_lock, sweep, sweeping_a_link_touches_nothing_and_warns};
use crate::host;
use crate::test_support::fixtures::unicode_home;

#[skuld::test]
fn sweeping_a_run_dir_that_is_a_junction_touches_nothing_and_warns(#[fixture(unicode_home)] home: &Path) {
    sweeping_a_link_touches_nothing_and_warns(home, |target, link| junction::create(target, link).unwrap());
}

/// A delete that is running holds its claim file under the deleting name. The sweep leaves it to that delete.
#[skuld::test]
fn sweep_leaves_a_deleting_name_a_running_delete_holds_and_does_not_warn() {
    let run = run_with_a_lock();
    let deleting = run.path().join(format!("{STEM_A}.sock.hold.del"));
    let hold = host::current().hold_new(&deleting).unwrap();

    assert_eq!(sweep(run.path()), Vec::<String>::new());

    assert!(deleting.exists());
    drop(hold);
}
