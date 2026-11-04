//! The Windows host against the real hypervisor platform.

use crate::Arch;
use crate::host::{self, Accel};
use crate::test_groups::{Group, vm_group};

#[skuld::test]
fn whpx_probe_reports_present_on_a_whpx_host(#[fixture(vm_group)] _group: &Group) {
    match host::current().accelerator(Arch::Amd64) {
        Ok(accel) => assert_eq!(accel, Accel::Whpx),
        Err(unavailable) => panic!(
            "this host has no WHPX, which the VM tests need: {unavailable}; set DEVVM_TEST_VM=0 to turn the group off"
        ),
    }
}

#[skuld::test]
fn guest_reset_is_available_on_this_host(#[fixture(vm_group)] _group: &Group) {
    if let Err(unavailable) = host::current().guest_reset(Arch::Amd64) {
        panic!(
            "this host cannot reset a running guest (WHvResetPartition), which the reboot tests need: {unavailable}; \
             set DEVVM_TEST_VM=0 to turn the group off"
        );
    }
}
