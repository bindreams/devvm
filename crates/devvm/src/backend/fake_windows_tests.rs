use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::FakeTarget;
use crate::ErrorKind;
use crate::backend::contract::{self, ContractTarget};
use crate::backend::{Backend, Endpoint, Machine, MachineRecord, MachineSpec, Storage};
use crate::contain::VmProcess;
use crate::store::layers::{ChainLease, LayerStore};
use crate::test_support::fake_vm::{STARTED, connect_when_up};
use crate::test_support::fixtures::anna_home;

/// The processes of the machines that `await_booted` was called for.
type Awaited = Arc<Mutex<HashSet<u32>>>;

/// A backend like QEMU in two respects: a frozen machine has no endpoints yet, and asking for them is a mistake; and a
/// machine that has not booted yet cannot be shut down gracefully, so a shutdown before `await_booted` is a mistake.
struct EndpointsAfterResume(Arc<dyn Backend>, Awaited);

impl Backend for EndpointsAfterResume {
    fn launch(&self, spec: &MachineSpec) -> crate::Result<Box<dyn Machine>> {
        Ok(Box::new(Strict {
            inner: self.0.launch(spec)?,
            resumed: false,
            awaited: self.1.clone(),
        }))
    }

    fn attach(&self, rec: &MachineRecord) -> crate::Result<Box<dyn Machine>> {
        Ok(Box::new(Strict {
            inner: self.0.attach(rec)?,
            resumed: true,
            awaited: self.1.clone(),
        }))
    }

    fn storage(&self) -> &dyn Storage {
        self.0.storage()
    }

    fn can_save_memory(&self) -> bool {
        false
    }
}

struct Strict {
    inner: Box<dyn Machine>,
    resumed: bool,
    awaited: Awaited,
}

impl Machine for Strict {
    fn record(&self) -> MachineRecord {
        self.inner.record()
    }

    fn ssh_endpoints(&self) -> &[(Endpoint, SocketAddr)] {
        assert!(
            self.resumed,
            "the endpoints of a machine that has not run are not known"
        );
        self.inner.ssh_endpoints()
    }

    fn serial_log(&self) -> &Path {
        self.inner.serial_log()
    }

    fn resume(&mut self) -> crate::Result<()> {
        self.inner.resume()?;
        self.resumed = true;
        Ok(())
    }

    fn release(&mut self) -> crate::Result<()> {
        self.inner.release()
    }

    fn shutdown(&mut self) -> crate::Result<()> {
        assert!(
            self.awaited.lock().unwrap().contains(&self.record().process.pid),
            "shutdown before the boot was awaited"
        );
        self.inner.shutdown()
    }

    fn kill(&mut self) -> crate::Result<()> {
        self.inner.kill()
    }
}

/// The fake target behind [`EndpointsAfterResume`]. Its `await_booted` is only valid on a running machine, and counts.
struct StrictTarget {
    inner: FakeTarget,
    booted_waits: AtomicUsize,
    awaited: Awaited,
}

impl StrictTarget {
    fn new() -> StrictTarget {
        StrictTarget {
            inner: FakeTarget::new(),
            booted_waits: AtomicUsize::new(0),
            awaited: Awaited::default(),
        }
    }
}

impl ContractTarget for StrictTarget {
    fn backend(&self) -> Arc<dyn Backend> {
        Arc::new(EndpointsAfterResume(self.inner.backend(), self.awaited.clone()))
    }

    fn parent_layer(&self, store: &LayerStore) -> ChainLease {
        self.inner.parent_layer(store)
    }

    fn spec(&self, root: &Path, n: u32) -> MachineSpec {
        self.inner.spec(root, n)
    }

    fn launch_elsewhere(&self, spec: &MachineSpec) -> crate::Result<MachineRecord> {
        self.inner.launch_elsewhere(spec)
    }

    fn shutdown_elsewhere(&self, record: &MachineRecord) -> crate::Result<()> {
        self.inner.shutdown_elsewhere(record)
    }

    fn has_run(&self, spec: &MachineSpec) -> bool {
        self.inner.has_run(spec)
    }

    fn leftovers(&self, spec: &MachineSpec) -> Vec<PathBuf> {
        self.inner.leftovers(spec)
    }

    fn await_booted(&self, machine: &dyn Machine) -> crate::Result<()> {
        // A machine that has not been resumed has no endpoints, and the call panics.
        assert!(!machine.ssh_endpoints().is_empty(), "the machine was not resumed");
        let record = machine.record();
        assert!(
            VmProcess::attach(&record.process).unwrap().is_some(),
            "the machine is not running"
        );
        self.booted_waits.fetch_add(1, Ordering::SeqCst);
        self.awaited.lock().unwrap().insert(record.process.pid);
        self.inner.await_booted(machine)
    }
}

/// The fake target with a helper process that fails: the shutdown helper, or the wait for a boot.
struct FailingTarget {
    inner: FakeTarget,
    shutdown_fails: bool,
    boot_fails: bool,
    launched_elsewhere: Mutex<Vec<MachineRecord>>,
}

impl FailingTarget {
    fn new(shutdown_fails: bool, boot_fails: bool) -> FailingTarget {
        FailingTarget {
            inner: FakeTarget::new(),
            shutdown_fails,
            boot_fails,
            launched_elsewhere: Mutex::new(Vec::new()),
        }
    }
}

impl ContractTarget for FailingTarget {
    fn backend(&self) -> Arc<dyn Backend> {
        self.inner.backend()
    }

    fn parent_layer(&self, store: &LayerStore) -> ChainLease {
        self.inner.parent_layer(store)
    }

    fn spec(&self, root: &Path, n: u32) -> MachineSpec {
        self.inner.spec(root, n)
    }

    fn launch_elsewhere(&self, spec: &MachineSpec) -> crate::Result<MachineRecord> {
        let record = self.inner.launch_elsewhere(spec)?;
        self.launched_elsewhere.lock().unwrap().push(record.clone());
        Ok(record)
    }

    fn shutdown_elsewhere(&self, record: &MachineRecord) -> crate::Result<()> {
        if self.shutdown_fails {
            return Err(crate::Error::new(ErrorKind::Internal, "the shutdown helper failed"));
        }
        self.inner.shutdown_elsewhere(record)
    }

    fn has_run(&self, spec: &MachineSpec) -> bool {
        self.inner.has_run(spec)
    }

    fn leftovers(&self, spec: &MachineSpec) -> Vec<PathBuf> {
        self.inner.leftovers(spec)
    }

    fn await_booted(&self, machine: &dyn Machine) -> crate::Result<()> {
        if self.boot_fails {
            return Err(crate::Error::new(ErrorKind::Internal, "the machine never booted"));
        }
        self.inner.await_booted(machine)
    }
}

/// Runs `function` over `target`, which must fail it, and checks that it failed and left no machine running.
fn fails_and_leaves_no_machine(target: &FailingTarget, home: &Path, function: fn(&dyn ContractTarget, &Path)) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| function(target, home)));
    assert!(outcome.is_err(), "the helper failed, and the contract function did not");
    let machines = target.launched_elsewhere.lock().unwrap().clone();
    assert_eq!(machines.len(), 1);
    for machine in machines {
        assert!(
            VmProcess::attach(&machine.process).unwrap().is_none(),
            "a failed contract function left its released machine running"
        );
    }
}

#[skuld::test]
fn a_failing_shutdown_helper_fails_its_function_and_leaves_no_machine(#[fixture(anna_home)] home: &Path) {
    fails_and_leaves_no_machine(
        &FailingTarget::new(true, false),
        home,
        contract::shutdown_from_another_process_stops_the_machine,
    );
}

#[skuld::test]
fn a_failing_assertion_after_a_release_leaves_no_machine(#[fixture(anna_home)] home: &Path) {
    fails_and_leaves_no_machine(
        &FailingTarget::new(false, true),
        home,
        contract::a_released_machine_outlives_its_launcher_and_attach_finds_it,
    );
}

#[skuld::test]
fn no_contract_function_reads_an_endpoint_before_resume(#[fixture(anna_home)] home: &Path) {
    let target = StrictTarget::new();
    let functions: [fn(&dyn ContractTarget, &Path); 8] = [
        contract::launch_reports_a_live_process_and_loopback_endpoints,
        contract::a_frozen_machine_has_not_run_and_kill_leaves_nothing,
        contract::a_released_machine_outlives_its_launcher_and_attach_finds_it,
        contract::kill_returns_only_after_the_process_has_exited,
        contract::attach_to_a_dead_machine_fails,
        contract::shutdown_stops_the_process,
        contract::a_running_machine_leaves_no_leftovers_after_kill_or_shutdown,
        contract::concurrent_launches_get_distinct_processes_and_endpoints,
    ];
    for (n, function) in functions.into_iter().enumerate() {
        function(&target, &home.join(n.to_string()));
    }
}

#[skuld::test]
fn the_shutdown_functions_wait_for_a_boot_before_the_first_shutdown(#[fixture(anna_home)] home: &Path) {
    let target = StrictTarget::new();
    contract::shutdown_stops_the_process(&target, &home.join("a"));
    assert_eq!(target.booted_waits.load(Ordering::SeqCst), 1);
    contract::a_released_machine_outlives_its_launcher_and_attach_finds_it(&target, &home.join("b"));
    assert_eq!(
        target.booted_waits.load(Ordering::SeqCst),
        2,
        "the attached machine is awaited"
    );
}

#[skuld::test]
fn a_shutdown_before_the_boot_was_awaited_fails_the_strict_target(#[fixture(anna_home)] home: &Path) {
    let target = StrictTarget::new();
    let env = crate::store::layers::layers_tests::env(home);
    let parent = target.parent_layer(&env.store);
    let spec = target.spec(home, 0);
    std::fs::create_dir_all(spec.disk.parent().unwrap()).unwrap();
    let backend = target.backend();
    backend
        .storage()
        .create_instance_disk(parent.tip().dir(), &spec.disk, 1 << 20)
        .unwrap();
    let mut machine = backend.launch(&spec).unwrap();
    machine.resume().unwrap();

    let early = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| machine.shutdown()));
    assert!(
        early.is_err(),
        "the strict machine allowed a shutdown before await_booted"
    );
    target.await_booted(&*machine).unwrap();
    machine.shutdown().expect("a shutdown after the wait is fine");
}

#[skuld::test]
fn fake_has_run_only_after_the_process_runs(#[fixture(anna_home)] home: &Path) {
    let target = FakeTarget::new();
    let env = crate::store::layers::layers_tests::env(home);
    let parent = target.parent_layer(&env.store);
    let spec = target.spec(home, 0);
    std::fs::create_dir_all(spec.disk.parent().unwrap()).unwrap();
    target
        .backend()
        .storage()
        .create_instance_disk(parent.tip().dir(), &spec.disk, 1 << 20)
        .unwrap();

    let mut machine = target.backend().launch(&spec).unwrap();
    assert!(!target.has_run(&spec));
    machine.resume().unwrap();
    let port = machine.ssh_endpoints()[0].1.port();
    let vm = crate::contain::VmProcess::attach(&machine.record().process)
        .unwrap()
        .unwrap();
    connect_when_up(&vm, port);
    assert!(target.has_run(&spec));
    for leftover in target.leftovers(&spec) {
        assert!(leftover.exists(), "a running fake machine has {}", leftover.display());
    }
    target
        .await_booted(&*machine)
        .expect("a fake machine is booted once it runs");
    machine.kill().unwrap();
}

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
fn fake_shutdown_from_another_process_stops_the_machine(#[fixture(anna_home)] home: &Path) {
    contract::shutdown_from_another_process_stops_the_machine(&FakeTarget::new(), home);
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
fn fake_a_running_machine_leaves_no_leftovers_after_kill_or_shutdown(#[fixture(anna_home)] home: &Path) {
    contract::a_running_machine_leaves_no_leftovers_after_kill_or_shutdown(&FakeTarget::new(), home);
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
