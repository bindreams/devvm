//! Reaching a running guest: instance keys, `ssh_config` entries and the embedded SSH client.

pub(crate) mod exec;
#[cfg(test)]
#[path = "access/heap_tests.rs"]
pub(crate) mod heap_tests;
pub(crate) mod keys;
pub(crate) mod paths;
pub(crate) mod runtime;
pub(crate) mod session;
pub(crate) mod sftp;
pub(crate) mod ssh_config;

#[allow(unused_imports, reason = "used by the engine in the next PR")]
pub(crate) use exec::ExecEnd;
#[allow(unused_imports, reason = "used by the engine in the next PR")]
pub(crate) use session::{BootBound, Keepalive, SshSession, SshTarget, VmExit};
