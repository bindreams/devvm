//! Starting a machine for an instance: the spec, the frozen launch, the record, the resume.

use std::sync::Arc;

use crate::access::keys::{InstanceKeys, client_key_path};
use crate::access::ssh_config::SshConfigEntry;
use crate::access::{SshSession, SshTarget};
use crate::backend::{Endpoint, Machine, MachineRecord, MachineSpec};
use crate::contain::Lifetime;
use crate::engine::keys::{ensure_keys, keys_dir, target_for};
use crate::engine::ready::connect_ready;
use crate::engine::{Engine, LaunchStep};
use crate::guest::{GuestOs, SeedKeys, SeedSpec};
use crate::instance::dir::{InstanceDir, OpExclusive};
use crate::vm::{remove_leftovers, warn_for};
use crate::{Error, ErrorKind, Result};

/// A machine that runs and has answered: still contained, and reached through `session`.
pub(crate) struct Started {
    pub machine: Box<dyn Machine>,
    pub session: SshSession,
}

/// Launches the machine of `dir` unless it already runs (`None`), and waits until the guest answers. The machine comes
/// back resumed and still contained, with the session that found the guest ready.
///
/// Until the caller releases the machine, it dies with this process. A devvm killed after the launch returns therefore
/// leaves no VM running. One killed inside the launch, between cosca creating the frozen process and putting it into
/// its job, leaves a frozen process nothing kills and nothing records. A failure at any step after the resume kills
/// the machine and clears its record.
pub(super) fn launch(
    engine: &Engine,
    dir: &InstanceDir,
    op: &OpExclusive,
    lifetime: Lifetime,
) -> Result<Option<Started>> {
    let Some(Launched {
        mut machine,
        running,
        keys,
        guest,
    }) = launch_machine(engine, dir, op, lifetime)?
    else {
        return Ok(None);
    };
    match await_ready(engine, dir, &keys, &*guest, &running) {
        Ok(session) => Ok(Some(Started { machine, session })),
        Err(e) => Err(abandon(engine, dir, op, &mut *machine, e)),
    }
}

/// A machine `launch_machine` resumed, with what the wait for its guest needs.
struct Launched {
    machine: Box<dyn Machine>,
    /// The machine's record once running, as written to the instance record.
    running: MachineRecord,
    keys: InstanceKeys,
    guest: Arc<dyn GuestOs>,
}

/// `launch` up to and including the resume; the guest is not yet awaited.
fn launch_machine(
    engine: &Engine,
    dir: &InstanceDir,
    op: &OpExclusive,
    lifetime: Lifetime,
) -> Result<Option<Launched>> {
    let backend = engine.backend()?;
    let record = dir.record()?;
    if let Some(machine) = &record.machine {
        if backend.find(machine)?.is_some() {
            return Ok(None);
        }
        // A stale record: the machine is dead, so its leftovers go.
        dir.retire_machine(op, machine, engine.host(), &mut warn_for(engine, dir))?;
    }

    let guest = engine.guests().os(record.guest)?;
    let accel = engine
        .host()
        .accelerator(record.arch)
        .map_err(|unavailable| Error::new(ErrorKind::Unavailable, unavailable.to_string()))?;
    let keys = ensure_keys(dir, op, engine.host(), &mut warn_for(engine, dir))?;
    let seed_dir = dir.path().join("seed");
    std::fs::create_dir_all(&seed_dir)
        .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", seed_dir.display()), e))?;
    let seed = guest.instance_seed(
        engine.host(),
        &SeedSpec {
            hostname: dir.name().as_str().to_owned(),
            dir: seed_dir,
            instance_id: *dir.id(),
            keys: SeedKeys {
                client_public: keys.client_public.clone(),
                host_private: keys.host_private.clone(),
                host_public: keys.host_public.clone(),
            },
        },
    )?;
    let spec = MachineSpec {
        id: *dir.id(),
        dir: dir.path().to_path_buf(),
        arch: record.arch,
        accel,
        cpus: record.cpus,
        memory: record.memory,
        network: record.network,
        disk: dir.disk(),
        media: vec![seed.media],
        profile: guest.device_profile(),
        firmware: None,
        forwards: vec![Endpoint::Normal],
        serial_log: dir.serial_log(),
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
    Ok(Some(Launched {
        machine,
        running: running_record,
        keys,
        guest,
    }))
}

/// Writes the instance's `ssh_config` entry for plain `ssh` and editors: the top-level `config` and the entry for
/// `target`. A failed `config` does not keep the entry from being written (an editor may include `<name>.conf` on its
/// own); the first failure is returned, with the second named after it. The embedded client does not read it, so a
/// start reports a failure and goes on; `Instance::ssh_config` does not.
pub(crate) fn write_ssh_entry(engine: &Engine, dir: &InstanceDir, target: &SshTarget) -> Result<()> {
    let configs = engine.ssh_configs();
    let top_level = configs.ensure_top_level();
    let entry = SshConfigEntry {
        name: dir.name().clone(),
        id: *dir.id(),
        port: target.addr.port(),
        identity: client_key_path(&keys_dir(dir)),
        known_hosts: dir.path().join("known_hosts"),
    };
    match (top_level, configs.write_entry(&entry, &target.host_public)) {
        (Err(first), Err(second)) => Err(first.together_with(second.message())),
        (Err(first), Ok(())) => Err(first),
        (Ok(()), written) => written,
    }
}

fn await_ready(
    engine: &Engine,
    dir: &InstanceDir,
    keys: &InstanceKeys,
    guest: &dyn GuestOs,
    running: &MachineRecord,
) -> Result<SshSession> {
    let target = target_for(dir.name(), running, keys)?;
    if let Err(e) = write_ssh_entry(engine, dir, &target) {
        warn_for(engine, dir)(format!("cannot write the ssh configuration: {}", e.message()));
    }
    connect_ready(engine, guest, dir.name(), &target, running)
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
    if let Err(clear) = dir.retire_machine(op, &machine.record(), engine.host(), &mut warn_for(engine, dir)) {
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
