use std::path::Path;

use super::FakeTarget;
use crate::backend::contract::{self, ContractTarget};
use crate::test_support::fake_vm::STARTED;
use crate::test_support::fixtures::anna_home;

#[skuld::test]
fn fake_launch_reports_a_live_process_and_loopback_endpoints(#[fixture(anna_home)] home: &Path) {
    contract::launch_reports_a_live_process_and_loopback_endpoints(&FakeTarget::new(), home);
}

#[skuld::test]
fn fake_a_frozen_machine_has_not_run_and_kill_leaves_nothing(#[fixture(anna_home)] home: &Path) {
    let target = FakeTarget::new();
    contract::a_frozen_machine_has_not_run_and_kill_leaves_nothing(&target, home);
    let state_dir = target.spec(home, 0).serial_log.parent().unwrap().to_path_buf();
    assert!(
        !state_dir.join(STARTED).exists(),
        "a frozen machine never wrote its marker"
    );
}

#[skuld::test]
fn fake_a_released_machine_outlives_its_launcher_and_attach_finds_it(#[fixture(anna_home)] home: &Path) {
    contract::a_released_machine_outlives_its_launcher_and_attach_finds_it(&FakeTarget::new(), home);
}

#[skuld::test]
fn fake_kill_returns_only_after_the_process_has_exited(#[fixture(anna_home)] home: &Path) {
    contract::kill_returns_only_after_the_process_has_exited(&FakeTarget::new(), home);
}

#[skuld::test]
fn fake_attach_to_a_dead_machine_fails(#[fixture(anna_home)] home: &Path) {
    contract::attach_to_a_dead_machine_fails(&FakeTarget::new(), home);
}

#[skuld::test]
fn fake_shutdown_stops_the_process(#[fixture(anna_home)] home: &Path) {
    contract::shutdown_stops_the_process(&FakeTarget::new(), home);
}

#[skuld::test]
fn fake_concurrent_launches_get_distinct_processes_and_endpoints(#[fixture(anna_home)] home: &Path) {
    contract::concurrent_launches_get_distinct_processes_and_endpoints(&FakeTarget::new(), home);
}

#[skuld::test]
fn fake_counts_every_launch(#[fixture(anna_home)] home: &Path) {
    let target = FakeTarget::new();
    assert_eq!(target.launches(), 0);
    contract::launch_reports_a_live_process_and_loopback_endpoints(&target, home);
    assert_eq!(target.launches(), 1);
}
