use cosca::identity::ProcessId;

use super::WindowsWatch;
use crate::host::ProcessWatch;
use crate::test_support::manual_exit::ManualExit;

#[skuld::test]
fn the_process_running_this_test_has_not_exited() {
    let watch = WindowsWatch::open(ProcessId::current()).unwrap();

    assert!(!watch.has_exited().unwrap());
    assert!(
        !watch.pause().unwrap(),
        "a pause for a process that lives lasts out its retry period"
    );
}

#[skuld::test]
fn a_handle_that_signals_is_an_exit_and_ends_a_pause_at_once() {
    let process = ManualExit::new();
    let watch = process.watch();
    assert!(!watch.has_exited().unwrap());

    process.exit();

    assert!(watch.has_exited().unwrap());
    assert!(watch.pause().unwrap());
}
