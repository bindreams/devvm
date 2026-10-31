use cosca::identity::ProcessId;

use super::UnixWatch;
use crate::host::ProcessWatch;

#[skuld::test]
fn the_process_running_this_test_has_not_exited() {
    let watch = UnixWatch::new(ProcessId::current());

    assert!(!watch.has_exited().unwrap());
    assert!(!watch.pause().unwrap());
}
