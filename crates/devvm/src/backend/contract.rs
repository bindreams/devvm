//! The backend contract: properties every `Backend` must have, as functions a backend's own tests call with a
//! [`ContractTarget`]. The fake backend runs them here; the QEMU backend runs the same ones.
//!
//! The machine properties start a frozen process, which only Windows can do in 0.1.0, so a backend's tests run them
//! there only.
#![cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the machine properties run only where a process can be spawned frozen"
    )
)]

use std::collections::{BTreeMap, HashSet};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use crate::ErrorKind;
use crate::backend::{Backend, Machine, MachineRecord, MachineSpec};
use crate::contain::VmProcess;
use crate::store::layers::layers_tests::{Env, env, key, request};
use crate::store::layers::{Built, ChainLease, LayerStore};

pub(crate) trait ContractTarget {
    fn backend(&self) -> Arc<dyn Backend>;

    /// A published layer for instance disks to sit on.
    fn parent_layer(&self, store: &LayerStore) -> ChainLease;

    /// The `n`th machine under `root`; its lifetime is `OutlivesOwner`. Its disk does not exist yet.
    fn spec(&self, root: &Path, n: u32) -> MachineSpec;

    /// Another OS process launches the spec, resumes and releases it, prints the `MachineRecord`, and exits.
    fn launch_elsewhere(&self, spec: &MachineSpec) -> crate::Result<MachineRecord>;

    /// Another OS process attaches to the machine `record` names, waits until it has booted (`await_booted`), shuts
    /// it down and exits. Returns once that process has exited, with an error if it failed.
    fn shutdown_elsewhere(&self, record: &MachineRecord) -> crate::Result<()>;

    /// Whether the machine launched from `spec` has ever run. Observes it without an endpoint: a frozen machine's
    /// endpoints may not exist yet.
    fn has_run(&self, spec: &MachineSpec) -> bool;

    /// Whether the machine launched from `spec` was ended by being asked to (`Machine::shutdown`) and not by being
    /// killed. Observed from outside the backend: a killed machine never asked.
    fn ended_gracefully(&self, spec: &MachineSpec) -> bool;

    /// The files outside the instance directory that belong to the machine launched from `spec`, whether or not they
    /// exist now. The machine's record names them from the launch on, before the resume, and a machine that was killed
    /// or shut down leaves none of them.
    fn leftovers(&self, spec: &MachineSpec) -> Vec<PathBuf>;

    /// Waits until a resumed machine has booted far enough to act on a graceful shutdown. A machine that needs no
    /// guest keeps this default.
    fn await_booted(&self, _machine: &dyn Machine) -> crate::Result<()> {
        Ok(())
    }
}

/// A layer store under the test's root, and the parent layer instance disks sit on.
struct World {
    env: Env,
    parent: ChainLease,
}

fn world(target: &dyn ContractTarget, root: &Path) -> World {
    let env = env(root);
    let parent = target.parent_layer(&env.store);
    World { env, parent }
}

/// The `n`th spec, with its instance disk made by the backend's own storage.
fn prepared(target: &dyn ContractTarget, world: &World, root: &Path, n: u32) -> MachineSpec {
    let spec = target.spec(root, n);
    std::fs::create_dir_all(spec.disk.parent().unwrap()).unwrap();
    target
        .backend()
        .storage()
        .create_instance_disk(world.parent.tip().dir(), &spec.disk, 1 << 20)
        .unwrap();
    spec
}

fn first_endpoint(machine: &dyn Machine) -> SocketAddr {
    machine.ssh_endpoints().first().expect("a machine has an endpoint").1
}

/// The record names every leftover of `spec`'s machine; the engine persists this record before the resume.
fn assert_names_the_leftovers(target: &dyn ContractTarget, spec: &MachineSpec, record: &MachineRecord) {
    for leftover in target.leftovers(spec) {
        assert!(
            record.leftovers.contains(&leftover),
            "the record taken before the resume does not name {}",
            leftover.display()
        );
    }
}

fn assert_no_leftovers(target: &dyn ContractTarget, spec: &MachineSpec, after: &str) {
    for leftover in target.leftovers(spec) {
        assert!(!leftover.exists(), "{after} left {}", leftover.display());
    }
}

/// Ends the process of a released machine, by its recorded identity, when the test fails before it has: nothing else
/// holds a released machine, so a failed function would leave it running after the test run.
struct KillsOnDrop<'a>(&'a MachineRecord);

impl Drop for KillsOnDrop<'_> {
    fn drop(&mut self) {
        if let Ok(Some(mut vm)) = VmProcess::attach(&self.0.process) {
            let _ = vm.kill();
        }
        self.0.remove_leftovers(&mut |_| {});
    }
}

fn is_running(record: &MachineRecord) -> bool {
    VmProcess::attach(&record.process).unwrap().is_some()
}

/// Waits until `addr` accepts a connection. Ends with a failure if the process dies instead.
fn accepts_eventually(record: &MachineRecord, addr: SocketAddr) {
    while TcpStream::connect(addr).is_err() {
        assert!(
            is_running(record),
            "the machine exited before {addr} accepted a connection"
        );
        std::thread::yield_now();
    }
}

// Machines ============================================================================================================

pub(crate) fn launch_reports_a_live_process_and_loopback_endpoints(target: &dyn ContractTarget, root: &Path) {
    let world = world(target, root);
    let spec = prepared(target, &world, root, 0);
    let mut machine = target.backend().launch(&spec).unwrap();
    assert!(is_running(&machine.record()), "a launched machine has a live process");

    // The endpoints exist once the machine runs: on some backends the ports are chosen then.
    machine.resume().unwrap();
    let record = machine.record();
    assert!(!machine.ssh_endpoints().is_empty());
    for (_, addr) in machine.ssh_endpoints() {
        assert!(addr.ip().is_loopback(), "{addr} is not a loopback address");
    }
    let recorded: Vec<_> = record.ssh.iter().map(|e| (e.endpoint, e.addr)).collect();
    assert_eq!(recorded, machine.ssh_endpoints(), "the record names the endpoints");
    assert_eq!(record.serial_log, spec.serial_log, "the record names the serial log");
    machine.kill().unwrap();
}

pub(crate) fn a_frozen_machine_has_not_run_and_kill_leaves_nothing(target: &dyn ContractTarget, root: &Path) {
    let world = world(target, root);
    let spec = prepared(target, &world, root, 0);
    let mut machine = target.backend().launch(&spec).unwrap();
    let record = machine.record();
    assert_names_the_leftovers(target, &spec, &record);

    assert!(!target.has_run(&spec), "a frozen machine has not run");
    machine.kill().unwrap();
    assert!(!is_running(&record), "kill leaves no process");
    assert!(!target.has_run(&spec), "a killed frozen machine never ran");
    assert_no_leftovers(target, &spec, "kill");
}

/// How a running machine is ended.
#[derive(Clone, Copy, Debug)]
pub(crate) enum End {
    Kill,
    Shutdown,
}

/// A machine that ran and is then killed or shut down leaves nothing outside its directory. The record taken before
/// the resume already names the files it creates.
pub(crate) fn a_running_machine_leaves_no_leftovers_after_kill_or_shutdown(target: &dyn ContractTarget, root: &Path) {
    let world = world(target, root);
    for (n, end) in [End::Kill, End::Shutdown].into_iter().enumerate() {
        let spec = prepared(target, &world, root, n as u32);
        let mut machine = target.backend().launch(&spec).unwrap();
        let before_the_run = machine.record();
        assert_names_the_leftovers(target, &spec, &before_the_run);

        machine.resume().unwrap();
        accepts_eventually(&before_the_run, first_endpoint(&*machine));
        target.await_booted(&*machine).unwrap();
        // The files must exist while the machine runs, or "none left afterwards" proves nothing.
        let leftovers = target.leftovers(&spec);
        assert!(
            !leftovers.is_empty(),
            "a target names the files its running machine creates"
        );
        for leftover in &leftovers {
            assert!(leftover.exists(), "a running machine has {}", leftover.display());
        }
        match end {
            End::Kill => machine.kill().unwrap(),
            End::Shutdown => machine.shutdown().unwrap(),
        }
        assert!(
            !is_running(&before_the_run),
            "{end:?} returned while the process was still running"
        );
        assert_no_leftovers(target, &spec, &format!("{end:?}"));
        assert_eq!(
            target.ended_gracefully(&spec),
            matches!(end, End::Shutdown),
            "{end:?} and a graceful end are not the same thing"
        );
    }
}

pub(crate) fn a_released_machine_outlives_its_launcher_and_attach_finds_it(target: &dyn ContractTarget, root: &Path) {
    let world = world(target, root);
    let spec = prepared(target, &world, root, 0);
    let record = target.launch_elsewhere(&spec).unwrap();
    let _released = KillsOnDrop(&record);
    assert_names_the_leftovers(target, &spec, &record);

    // The launcher process has exited by now.
    assert!(is_running(&record), "a released machine survives its launcher");
    let mut machine = target.backend().attach(&record).unwrap();
    assert_eq!(machine.record(), record);
    assert_eq!(
        machine.serial_log(),
        spec.serial_log,
        "an attached machine knows its serial log"
    );
    accepts_eventually(&record, first_endpoint(&*machine));
    target.await_booted(&*machine).unwrap();
    assert_eq!(
        machine.resume().unwrap_err().kind(),
        ErrorKind::Internal,
        "an attached machine already runs"
    );
    machine.release().expect("releasing an attached machine is Ok");

    machine.kill().unwrap();
    assert!(!is_running(&record));
    assert_eq!(
        target.backend().attach(&record).err().unwrap().kind(),
        ErrorKind::NotFound
    );
}

pub(crate) fn kill_returns_only_after_the_process_has_exited(target: &dyn ContractTarget, root: &Path) {
    let world = world(target, root);
    let spec = prepared(target, &world, root, 0);
    let mut machine = target.backend().launch(&spec).unwrap();
    let record = machine.record();
    machine.resume().unwrap();
    accepts_eventually(&record, first_endpoint(&*machine));

    machine.kill().unwrap();
    assert!(
        !is_running(&record),
        "kill returned while the process was still running"
    );
    assert!(!target.ended_gracefully(&spec), "a killed machine was not asked to end");
}

pub(crate) fn attach_to_a_dead_machine_fails(target: &dyn ContractTarget, root: &Path) {
    let world = world(target, root);
    let spec = prepared(target, &world, root, 0);
    let mut machine = target.backend().launch(&spec).unwrap();
    let record = machine.record();
    machine.kill().unwrap();

    let error = target.backend().attach(&record).err().expect("the machine is dead");
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
}

pub(crate) fn shutdown_stops_the_process(target: &dyn ContractTarget, root: &Path) {
    let world = world(target, root);
    let spec = prepared(target, &world, root, 0);
    let mut machine = target.backend().launch(&spec).unwrap();
    let record = machine.record();
    machine.resume().unwrap();
    accepts_eventually(&record, first_endpoint(&*machine));
    target.await_booted(&*machine).unwrap();

    machine.shutdown().unwrap();
    assert!(
        !is_running(&record),
        "shutdown returned while the process was still running"
    );
    assert!(
        target.ended_gracefully(&spec),
        "shutdown ended the machine without asking it to"
    );
}

pub(crate) fn shutdown_from_another_process_stops_the_machine(target: &dyn ContractTarget, root: &Path) {
    let world = world(target, root);
    let spec = prepared(target, &world, root, 0);
    let record = target.launch_elsewhere(&spec).unwrap();
    let _released = KillsOnDrop(&record);
    assert_names_the_leftovers(target, &spec, &record);
    assert!(is_running(&record), "a released machine survives its launcher");

    // The other process asks the machine to shut down and returns once it has exited, or fails. Nothing here waits
    // for the process: a machine still running when the call returns fails the function.
    target.shutdown_elsewhere(&record).unwrap();
    assert!(
        !is_running(&record),
        "shutdown returned while the process was still running"
    );
    assert_no_leftovers(target, &spec, "a shutdown from another process");
    assert!(
        target.ended_gracefully(&spec),
        "a shutdown from another process ended the machine without asking it to"
    );
}

pub(crate) fn concurrent_launches_get_distinct_processes_and_endpoints(target: &dyn ContractTarget, root: &Path) {
    const COUNT: u32 = 4;
    let world = world(target, root);
    let specs: Vec<MachineSpec> = (0..COUNT).map(|n| prepared(target, &world, root, n)).collect();
    let backend = target.backend();
    let barrier = Barrier::new(specs.len());

    let mut machines: Vec<Box<dyn Machine>> = std::thread::scope(|scope| {
        let launched: Vec<_> = specs
            .iter()
            .map(|spec| {
                let (backend, barrier) = (&backend, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    backend.launch(spec).unwrap()
                })
            })
            .collect();
        launched.into_iter().map(|launched| launched.join().unwrap()).collect()
    });

    let records: Vec<MachineRecord> = machines.iter().map(|machine| machine.record()).collect();
    let processes: HashSet<_> = records.iter().map(|r| (r.process.pid, r.process.token)).collect();
    // The endpoints exist once the machines run.
    for machine in &mut machines {
        machine.resume().unwrap();
    }
    let endpoints: HashSet<_> = machines.iter().map(|m| first_endpoint(&**m)).collect();
    for machine in &mut machines {
        machine.kill().unwrap();
    }
    assert_eq!(processes.len(), COUNT as usize, "every machine has its own process");
    assert_eq!(endpoints.len(), COUNT as usize, "every machine has its own endpoint");
}

// Storage =============================================================================================================

/// Every file under `dir` with its bytes, except the layer store's lock files: they hold no data and the build in
/// progress has them locked.
fn files_under(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        // A file the check cannot see is a file it cannot vouch for: unreadable is a failure, not a skip.
        let entries = std::fs::read_dir(&next).unwrap_or_else(|e| panic!("cannot list {}: {e}", next.display()));
        for entry in entries {
            let entry = entry.unwrap_or_else(|e| panic!("cannot list {}: {e}", next.display()));
            let path = entry.path();
            let kind = entry
                .file_type()
                .unwrap_or_else(|e| panic!("cannot tell what {} is: {e}", path.display()));
            if path.file_name().is_some_and(|name| name == ".locks") {
                continue;
            } else if kind.is_dir() {
                pending.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
                found.insert(path, bytes);
            }
        }
    }
    found
}

pub(crate) fn storage_writes_only_into_staging(target: &dyn ContractTarget, root: &Path) {
    let world = world(target, root);
    let spec = prepared(target, &world, root, 0);
    let disk_before = std::fs::read(&spec.disk).unwrap();
    let layers = world.env.dirs.layers();
    let backend = target.backend();

    let committed = world
        .env
        .store
        .build(&request(key(2)), Some(&world.parent), |staging| {
            let before = files_under(&layers);
            backend.storage().commit_layer(&spec.disk, staging.dir)?;
            let after = files_under(&layers);

            for (path, bytes) in &before {
                assert_eq!(after.get(path), Some(bytes), "{} was modified", path.display());
            }
            let new: Vec<_> = after.keys().filter(|path| !before.contains_key(*path)).collect();
            assert!(!new.is_empty(), "committing a layer wrote nothing");
            for path in new {
                assert!(
                    path.starts_with(staging.dir.path()),
                    "{} is outside the staging directory",
                    path.display()
                );
            }
            Ok(Built::default())
        })
        .unwrap();

    assert_eq!(
        std::fs::read(&spec.disk).unwrap(),
        disk_before,
        "the instance disk is only read"
    );
    assert_eq!(committed.layers().len(), world.parent.layers().len() + 1);
}
