//! The Windows host against the real hypervisor platform.

use crate::Arch;
use crate::host::{self, Accel};
use crate::vm_gate::vm_tests_enabled;

#[skuld::test(requires = [vm_tests_enabled])]
fn whpx_probe_reports_present_on_a_whpx_host() {
    match host::current().accelerator(Arch::Amd64) {
        Ok(accel) => assert_eq!(accel, Accel::Whpx),
        Err(unavailable) => panic!("this host has no WHPX, which the VM tests need: {unavailable}"),
    }
}

#[skuld::test(requires = [vm_tests_enabled])]
fn guest_reset_is_available_on_this_host() {
    if let Err(unavailable) = host::current().guest_reset(Arch::Amd64) {
        panic!(
            "this host cannot reset a running guest (WHvResetPartition), which the reboot tests need: {unavailable}"
        );
    }
}
