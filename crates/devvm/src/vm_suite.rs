//! Tests that boot QEMU or run one of its tools. Every test here carries the `vm` label (`SKULD_LABELS=vm` selects them)
//! and joins the `vm_group` test group: it fails naming what is missing unless the group is turned off
//! (`vm_group::OPT_OUT`). The exception is the no-VM test in `fixture.rs`, which has no label and joins `unicode_base`
//! only.

use crate::test_harness::VM;

skuld::default_labels!(VM);

pub(crate) mod backend;
mod build;
mod fixture;
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
