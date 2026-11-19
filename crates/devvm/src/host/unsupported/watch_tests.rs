use cosca::identity::ProcessId;

use super::UnixWatch;
use crate::contain::contain_tests::Sleeper;
use crate::host::ProcessWatch;

#[skuld::test]
fn a_wait_watch_of_the_process_running_this_test_says_it_has_not_exited() {
    let watch = UnixWatch::new(ProcessId::current()).unwrap();

    assert!(!watch.has_exited().unwrap());
    assert!(!watch.pause().unwrap());
}

#[skuld::test]
fn a_wait_watch_of_a_process_that_is_gone_says_it_has_exited() {
    let sleeper = Sleeper::spawn();
    let id = ProcessId::try_from(&sleeper.record()).unwrap();
    sleeper.finish();

    let watch = UnixWatch::new(id).unwrap();

    assert!(watch.has_exited().unwrap());
    assert!(watch.pause().unwrap());
}
