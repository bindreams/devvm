//! Tests that boot QEMU or run one of its tools. Every test here carries the `vm` label (`SKULD_LABELS=vm` selects them)
//! and goes through the VM gate: it fails naming what is missing unless `DEVVM_SKIP_VM_TESTS=1`.

use crate::vm_gate::VM;

skuld::default_labels!(VM);

pub(crate) mod backend;
mod host;
mod qmp;
mod storage;

/// What the tests ask the OS about a process they did not write.
#[cfg(windows)]
#[path = "vm_suite/os_windows.rs"]
mod os;
#[cfg(not(windows))]
#[path = "vm_suite/os_other.rs"]
mod os;
