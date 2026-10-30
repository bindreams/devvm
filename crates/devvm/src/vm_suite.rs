//! Tests that boot QEMU or run one of its tools. Every test here carries the `vm` label (`SKULD_LABELS=vm` selects them)
//! and goes through the VM gate: it fails naming what is missing unless `DEVVM_SKIP_VM_TESTS=1`.

use crate::vm_gate::VM;

skuld::default_labels!(VM);

mod qmp;
