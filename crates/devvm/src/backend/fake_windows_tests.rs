use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::FakeTarget;
use crate::backend::contract::{self, ContractTarget};
use crate::backend::{Backend, Endpoint, Machine, MachineRecord, MachineSpec, Storage};
use crate::store::layers::{ChainLease, LayerStore};
use crate::test_support::fake_vm::{STARTED, connect_when_up};
use crate::test_support::fixtures::anna_home;

/// A backend like QEMU in one respect: a frozen machine has no endpoints yet, and asking for them is a mistake.
struct EndpointsAfterResume(Arc<dyn Backend>);

impl Backend for EndpointsAfterResume {
    fn launch(&self, spec: &MachineSpec) -> crate::Result<Box<dyn Machine>> {
        Ok(Box::new(Strict {
            inner: self.0.launch(spec)?,
            resumed: false,
        }))
    }

    fn attach(&self, rec: &MachineRecord) -> crate::Result<Box<dyn Machine>> {
        self.0.attach(rec)
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
        self.inner.shutdown()
    }

    fn kill(&mut self) -> crate::Result<()> {
        self.inner.kill()
    }
}

/// The fake target behind [`EndpointsAfterResume`], counting how often the contract waits for a boot.
struct StrictTarget {
    inner: FakeTarget,
    booted_waits: AtomicUsize,
}

impl StrictTarget {
    fn new() -> StrictTarget {
        StrictTarget {
            inner: FakeTarget::new(),
            booted_waits: AtomicUsize::new(0),
        }
    }
}

impl ContractTarget for StrictTarget {
    fn backend(&self) -> Arc<dyn Backend> {
        Arc::new(EndpointsAfterResume(self.inner.backend()))
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
        self.booted_waits.fetch_add(1, Ordering::SeqCst);
        self.inner.await_booted(machine)
    }
}

#[skuld::test]
fn the_launch_and_frozen_functions_read_no_endpoint_before_resume(#[fixture(anna_home)] home: &Path) {
    let target = StrictTarget::new();
    contract::launch_reports_a_live_process_and_loopback_endpoints(&target, &home.join("a"));
    contract::a_frozen_machine_has_not_run_and_kill_leaves_nothing(&target, &home.join("b"));
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
    assert!(target.leftovers(&spec).is_empty());
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
