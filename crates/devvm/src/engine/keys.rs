//! An instance's SSH keys and the connection target built from them.

use std::path::PathBuf;

use crate::access::SshTarget;
use crate::access::keys::InstanceKeys;
use crate::backend::{Endpoint, Machine, MachineRecord};
use crate::engine::Engine;
use crate::host::Host;
use crate::instance::dir::{InstanceDir, OpExclusive, OpHeld};
use crate::instance::expiry::warn_if_expired;
use crate::instance::name::InstanceName;
use crate::instance::record::InstanceRecord;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "keys_tests.rs"]
mod keys_tests;

/// The account every guest's seed creates; the `User` of the `ssh_config` entry names it too.
pub(crate) const SSH_USER: &str = "user";

pub(crate) fn keys_dir(dir: &InstanceDir) -> PathBuf {
    dir.path().join("keys")
}

/// The keys of `dir`: generated at the first call and read at every later one, so a restarted instance keeps its host
/// key and the seed it feeds stays the same. What the write could not clean up goes to `warn`.
///
/// A set with a file missing is an error naming the directory, never a new set: `InstanceKeys::read` answers
/// `NotFound` for it as for no keys at all, and the write that follows refuses a directory that is not empty.
pub(crate) fn ensure_keys(
    dir: &InstanceDir,
    _op: &OpExclusive,
    host: &dyn Host,
    warn: &mut dyn FnMut(String),
) -> Result<InstanceKeys> {
    let path = keys_dir(dir);
    match InstanceKeys::read(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let keys = InstanceKeys::generate()?;
            for warning in keys.write(host, &path)? {
                warn(warning);
            }
            Ok(keys)
        }
        read => read,
    }
}

fn not_running(record: &InstanceRecord) -> Error {
    Error::new(
        ErrorKind::Unavailable,
        format!("instance {} is not running: start it", record.name),
    )
}

/// What a process needs to connect to `machine`, the machine of instance `name`, with `keys`.
pub(crate) fn target_for(name: &InstanceName, machine: &MachineRecord, keys: &InstanceKeys) -> Result<SshTarget> {
    let Some(endpoint) = machine.ssh.iter().find(|e| e.endpoint == Endpoint::Normal) else {
        return Err(Error::new(
            ErrorKind::Internal,
            format!("the machine of instance {name} reports no SSH endpoint"),
        ));
    };
    Ok(SshTarget {
        addr: endpoint.addr,
        user: SSH_USER.to_owned(),
        client_private: keys.client_private.clone(),
        host_public: keys.host_public.clone(),
        serial_log: machine.serial_log.clone(),
    })
}

/// The machine `dir` has now, found through the backend, and what a process needs to connect to it.
pub(crate) struct Running {
    pub machine: Box<dyn Machine>,
    /// The machine's record, as the instance record keeps it.
    pub record: MachineRecord,
    pub target: SshTarget,
}

/// The record of `dir`, read under the op lock. Warns first if the guest has expired: every operation that reaches the
/// guest reads its record here, once.
pub(crate) fn read_record(engine: &Engine, dir: &InstanceDir, _op: &impl OpHeld) -> Result<InstanceRecord> {
    let record = dir.record()?;
    warn_if_expired(engine, dir.name(), record.expires);
    Ok(record)
}

/// The machine `dir` has now, built from its record, the backend and the key files: the op lock keeps the starter from
/// rewriting any of them. A recorded machine that the backend does not find is not running: the record is stale.
pub(crate) fn read_target(engine: &Engine, dir: &InstanceDir, op: &impl OpHeld) -> Result<Running> {
    running_of(engine, dir, &read_record(engine, dir, op)?, op)
}

/// [`read_target`] from a `record` already read under `op`.
pub(crate) fn running_of(
    engine: &Engine,
    dir: &InstanceDir,
    record: &InstanceRecord,
    _op: &impl OpHeld,
) -> Result<Running> {
    // A stopped instance may be one that never started, and has no keys.
    let Some(machine) = &record.machine else {
        return Err(not_running(record));
    };
    let Some(found) = engine.backend()?.find(machine)? else {
        return Err(not_running(record));
    };
    let target = target_for(&record.name, machine, &InstanceKeys::read(&keys_dir(dir))?)?;
    Ok(Running {
        machine: found,
        record: machine.clone(),
        target,
    })
}
