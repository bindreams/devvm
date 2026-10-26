//! Starting a machine for an instance: the spec, the frozen launch, the record, the resume.

use crate::backend::{Endpoint, Machine, MachineSpec};
use crate::contain::{Lifetime, VmProcess};
use crate::engine::{Engine, LaunchStep};
use crate::guest::{SeedKeys, SeedSpec};
use crate::instance::dir::{InstanceDir, OpExclusive};
use crate::vm::remove_leftovers;
use crate::{Error, ErrorKind, Result};

/// Launches the machine of `dir` unless it already runs (`None`). The machine comes back resumed and still contained.
///
/// Order, so that a devvm process killed at any point leaves no VM running: launch frozen, record the machine in the
/// instance record, resume. Until the caller releases the machine, it dies with this process.
pub(super) fn launch(
    engine: &Engine,
    dir: &InstanceDir,
    op: &OpExclusive,
    lifetime: Lifetime,
) -> Result<Option<Box<dyn Machine>>> {
    let record = dir.record()?;
    if let Some(machine) = &record.machine {
        if VmProcess::attach(&machine.process)?.is_some() {
            return Ok(None);
        }
        // A stale record: the machine is dead, and its files must go before a new machine takes their place.
        remove_leftovers(engine, dir, machine);
    }

    let backend = engine.backend()?;
    let guest = engine.guests().os(record.guest)?;
    let accel = engine
        .host()
        .accelerator(record.arch)
        .map_err(|unavailable| Error::new(ErrorKind::Unavailable, unavailable.to_string()))?;
    let seed_dir = dir.path().join("seed");
    std::fs::create_dir_all(&seed_dir)
        .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", seed_dir.display()), e))?;
    let seed = guest.instance_seed(&SeedSpec {
        hostname: dir.name().as_str().to_owned(),
        dir: seed_dir,
        instance_id: *dir.id(),
        // Placeholder until the instance's keys exist (U4 generates them before the first start).
        keys: SeedKeys::default(),
    })?;
    let spec = MachineSpec {
        id: *dir.id(),
        dir: dir.path().to_path_buf(),
        arch: record.arch,
        accel,
        cpus: record.cpus,
        memory: record.memory,
        network: record.network,
        disk: dir.path().join("disk.img"),
        media: vec![seed.media],
        profile: guest.device_profile(),
        firmware: None,
        forwards: vec![Endpoint::Normal],
        serial_log: dir.path().join("serial.log"),
        lifetime,
    };

    let mut machine = backend.launch(&spec)?;
    let machine_record = machine.record();
    engine.hooks().reached(LaunchStep::Spawned, &machine_record);
    if let Err(e) = dir.update(op, &mut |record| record.machine = Some(machine_record.clone())) {
        // Not recorded, so nothing could find it again: it must not run.
        kill_unrecorded(engine, dir, &mut *machine);
        return Err(e);
    }
    engine.hooks().reached(LaunchStep::Recorded, &machine_record);
    if let Err(e) = machine.resume() {
        abandon(engine, dir, op, &mut *machine);
        return Err(e);
    }
    // A machine may learn its endpoints only by running, so its record is written once more.
    let running_record = machine.record();
    engine.hooks().reached(LaunchStep::Resumed, &running_record);
    if let Err(e) = dir.update(op, &mut |record| record.machine = Some(running_record.clone())) {
        abandon(engine, dir, op, &mut *machine);
        return Err(e);
    }
    Ok(Some(machine))
}

/// A recorded machine that must not run after all: kills it and clears the record that names it.
fn abandon(engine: &Engine, dir: &InstanceDir, op: &OpExclusive, machine: &mut dyn Machine) {
    kill_unrecorded(engine, dir, machine);
    if let Err(clear) = dir.update(op, &mut |record| record.machine = None) {
        engine.progress().message(&format!(
            "instance {} could not be started, and its record still names the machine ({}); the next devvm \
             command on it clears that",
            dir.name(),
            clear.message()
        ));
    }
}

fn kill_unrecorded(engine: &Engine, dir: &InstanceDir, machine: &mut dyn Machine) {
    match machine.kill() {
        Ok(()) => remove_leftovers(engine, dir, &machine.record()),
        Err(e) => engine.progress().message(&format!(
            "instance {}: the machine that failed to start could not be killed ({})",
            dir.name(),
            e.message()
        )),
    }
}
