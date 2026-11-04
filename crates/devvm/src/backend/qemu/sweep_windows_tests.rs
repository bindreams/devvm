use std::path::Path;

use super::sweep_tests::{
    STEM_A, deleting_name_of_a, run_with_a_lock, sweep, sweeping_a_link_touches_nothing_and_warns,
};
use crate::backend::qemu::qmp::QmpEndpoint;
use crate::host;
use crate::test_support::delete_pending::make_delete_pending;
use crate::test_support::fixtures::unicode_home;

#[skuld::test]
fn sweeping_a_run_dir_that_is_a_junction_touches_nothing_and_warns(#[fixture(unicode_home)] home: &Path) {
    sweeping_a_link_touches_nothing_and_warns(home, |target, link| junction::create(target, link).unwrap());
}

/// A delete that is running holds its claim file under the deleting name. The sweep leaves it to that delete.
#[skuld::test]
fn sweep_leaves_a_deleting_name_a_running_delete_holds_and_does_not_warn() {
    let run = run_with_a_lock();
    let deleting = deleting_name_of_a(run.path());
    let hold = host::current().hold_new(&deleting).unwrap();

    assert_eq!(sweep(run.path()), Vec::<String>::new());

    assert!(deleting.exists());
    drop(hold);
}

/// A claim file whose deletion is pending is held, or being deleted, by somebody else: the sweep leaves the machine's
/// files and does not warn. It does not read the claim file as absent, which would delete the lock beside it.
#[skuld::test]
fn sweep_leaves_a_launch_whose_claim_file_is_delete_pending_and_does_not_warn() {
    let run = run_with_a_lock();
    let endpoint = QmpEndpoint::of_launch(run.path(), STEM_A);
    let (claim, lock) = (endpoint.hold_path(), endpoint.lock_path());
    std::fs::write(&claim, b"pending").unwrap();
    std::fs::write(&lock, b"").unwrap();
    let keeper = make_delete_pending(&claim);

    assert_eq!(sweep(run.path()), Vec::<String>::new());

    assert!(claim.exists() && lock.exists());
    drop(keeper);
}
