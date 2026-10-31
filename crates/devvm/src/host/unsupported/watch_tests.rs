use cosca::identity::ProcessId;

use super::UnixWatch;
use crate::host::ProcessWatch;

#[skuld::test]
fn a_wait_watch_of_the_process_running_this_test_says_it_has_not_exited() {
    let watch = UnixWatch::new(ProcessId::current());

    assert!(!watch.has_exited().unwrap());
    assert!(!watch.pause().unwrap());
}
