//! Starting a machine for an instance: the spec, the frozen launch, the record, the resume.

use crate::backend::{Endpoint, Machine, MachineSpec};
use crate::contain::{Lifetime, VmProcess};
use crate::engine::{Engine, LaunchStep};
use crate::guest::SeedSpec;
use crate::instance::dir::{InstanceDir, OpExclusive};
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
    if let Some(machine) = &record.machine
        && VmProcess::attach(&machine.process)?.is_some()
    {
        return Ok(None);
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
    })?;
    let spec = MachineSpec {
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
        kill_unrecorded(engine, dir, &mut *machine);
        if let Err(clear) = dir.update(op, &mut |record| record.machine = None) {
            engine.progress().message(&format!(
                "instance {} could not be started, and its record still names the machine ({}); the next devvm \
                 command on it clears that",
                dir.name(),
                clear.message()
            ));
        }
        return Err(e);
    }
    engine.hooks().reached(LaunchStep::Resumed, &machine_record);
    Ok(Some(machine))
}

fn kill_unrecorded(engine: &Engine, dir: &InstanceDir, machine: &mut dyn Machine) {
    if let Err(e) = machine.kill() {
        engine.progress().message(&format!(
            "instance {}: the machine that failed to start could not be killed ({})",
            dir.name(),
            e.message()
        ));
    }
}
