//! Starting a machine for an instance: the spec, the frozen launch, the record, the resume.

use crate::backend::{Endpoint, Machine, MachineSpec};
use crate::contain::Lifetime;
use crate::engine::{Engine, LaunchStep};
use crate::guest::{SeedKeys, SeedSpec};
use crate::instance::dir::{InstanceDir, OpExclusive};
use crate::vm::{remove_leftovers, warn_for};
use crate::{Error, ErrorKind, Result};

/// Launches the machine of `dir` unless it already runs (`None`). The machine comes back resumed and still contained.
///
/// Order: launch frozen, record the machine in the instance record, resume. Until the caller releases the machine, it
/// dies with this process. A devvm killed after the launch returns therefore leaves no VM running. One killed inside
/// the launch, between cosca creating the frozen process and putting it into its job, leaves a frozen process nothing
/// kills and nothing records.
pub(super) fn launch(
    engine: &Engine,
    dir: &InstanceDir,
    op: &OpExclusive,
    lifetime: Lifetime,
) -> Result<Option<Box<dyn Machine>>> {
    let backend = engine.backend()?;
    let record = dir.record()?;
    if let Some(machine) = &record.machine {
        if backend.find(machine)?.is_some() {
            return Ok(None);
        }
        // A stale record: the machine is dead, so its leftovers go.
        dir.retire_machine(op, machine, &mut warn_for(engine, dir))?;
    }

    let guest = engine.guests().os(record.guest)?;
    let accel = engine
        .host()
        .accelerator(record.arch)
        .map_err(|unavailable| Error::new(ErrorKind::Unavailable, unavailable.to_string()))?;
    let seed_dir = dir.path().join("seed");
    std::fs::create_dir_all(&seed_dir)
        .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", seed_dir.display()), e))?;
    let seed = guest.instance_seed(
        engine.host(),
        &SeedSpec {
            hostname: dir.name().as_str().to_owned(),
            dir: seed_dir,
            instance_id: *dir.id(),
            // Placeholder until the instance's keys exist.
            keys: SeedKeys::default(),
        },
    )?;
    // Taken and persisted before the spawn: a launch that dies midway still used its number, so no later machine of the
    // instance is named like it. Incremented on the current record, under the op lock; at the last number the record is
    // rewritten unchanged.
    let mut taken = None;
    let current = dir.update(op, &mut |record| {
        taken = record.launches.checked_add(1);
        if let Some(next) = taken {
            record.launches = next;
        }
    })?;
    let launch = taken.ok_or_else(|| {
        Error::new(
            ErrorKind::Store,
            format!(
                "instance {} has launched {} times, which is as far as the count goes",
                dir.name(),
                current.launches
            ),
        )
    })?;
    let spec = MachineSpec {
        id: *dir.id(),
        launch,
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
        return Err(match kill_unrecorded(engine, dir, &mut *machine) {
            Ok(()) => e,
            Err(kill) => survivor(e, kill, "it was never recorded, so nothing can find it"),
        });
    }
    engine.hooks().reached(LaunchStep::Recorded, &machine_record);
    if let Err(e) = machine.resume() {
        return Err(abandon(engine, dir, op, &mut *machine, e));
    }
    // A machine may learn its endpoints only by running, so its record is written once more.
    let running_record = machine.record();
    engine.hooks().reached(LaunchStep::Resumed, &running_record);
    if let Err(e) = dir.update(op, &mut |record| record.machine = Some(running_record.clone())) {
        return Err(abandon(engine, dir, op, &mut *machine, e));
    }
    Ok(Some(machine))
}

/// A recorded machine that must not run after all: kills it and clears the record that names it. `cause` is why the
/// start failed and comes back as the error. A machine that cannot be killed may still run, so its record stays and the
/// next stop or remove of the instance ends it; the returned error then names both failures.
pub(super) fn abandon(
    engine: &Engine,
    dir: &InstanceDir,
    op: &OpExclusive,
    machine: &mut dyn Machine,
    cause: Error,
) -> Error {
    if let Err(kill) = machine.kill() {
        return survivor(
            cause,
            kill,
            "its record is kept, and the next stop or remove of the instance ends it",
        );
    }
    if let Err(clear) = dir.retire_machine(op, &machine.record(), &mut warn_for(engine, dir)) {
        engine.progress().message(&format!(
            "instance {} could not be started, and its record still names the machine ({}); the next devvm \
             command on it clears that",
            dir.name(),
            clear.message()
        ));
    }
    cause
}

/// Kills a machine that must not run, and deletes its leftovers once it is dead. The error is the failed kill.
fn kill_unrecorded(engine: &Engine, dir: &InstanceDir, machine: &mut dyn Machine) -> Result<()> {
    machine.kill()?;
    remove_leftovers(engine, dir, &machine.record());
    Ok(())
}

/// The error of a start that failed (`cause`) while the machine it had launched could not be killed (`kill`).
fn survivor(cause: Error, kill: Error, what_happens: &str) -> Error {
    Error::new(
        cause.kind(),
        format!(
            "{}; the machine that failed to start could not be killed either ({}) and may still run: {what_happens}",
            cause.message(),
            kill.message()
        ),
    )
    .with_source(kill)
}
