use super::{Hypervisor, accelerator, guest_reset};
use crate::Arch;
use crate::host::Accel;

/// A hypervisor platform that answers from a table.
struct Table {
    present: Result<bool, &'static str>,
    reset: Result<bool, &'static str>,
}

impl Hypervisor for Table {
    fn present(&self) -> Result<bool, String> {
        self.present.map_err(str::to_owned)
    }

    fn has_reset_partition(&self) -> Result<bool, String> {
        self.reset.map_err(str::to_owned)
    }
}

const RESET_TEXT: &str =
    "rebooting a guest needs Windows 11 or later (this host's WinHvPlatform.dll has no WHvResetPartition)";

#[skuld::test]
fn a_present_hypervisor_gives_whpx_for_amd64() {
    let table = Table {
        present: Ok(true),
        reset: Ok(true),
    };
    assert_eq!(accelerator(&table, Arch::Amd64).unwrap(), Accel::Whpx);
}

#[skuld::test]
fn arm64_is_software_emulation_and_asks_nothing() {
    let table = Table {
        present: Err("never asked"),
        reset: Err("never asked"),
    };
    assert_eq!(accelerator(&table, Arch::Arm64).unwrap(), Accel::Tcg);
}

#[skuld::test]
fn unavailable_text_names_the_feature() {
    let absent = Table {
        present: Ok(false),
        reset: Ok(false),
    };
    let missing = accelerator(&absent, Arch::Amd64).unwrap_err().missing;
    assert!(missing.contains("Windows Hypervisor Platform"), "{missing}");
    assert!(missing.contains("HypervisorPlatform"), "how to enable it: {missing}");

    let unloadable = Table {
        present: Err("cannot load WinHvPlatform.dll: 126"),
        reset: Ok(false),
    };
    let missing = accelerator(&unloadable, Arch::Amd64).unwrap_err().missing;
    assert!(missing.contains("Windows Hypervisor Platform"), "{missing}");
    assert!(
        missing.contains("cannot load WinHvPlatform.dll: 126"),
        "the cause is kept: {missing}"
    );
}

#[skuld::test]
fn guest_reset_is_ok_when_the_export_exists_and_names_windows_11_when_it_does_not() {
    let windows_11 = Table {
        present: Ok(true),
        reset: Ok(true),
    };
    assert!(guest_reset(&windows_11, Arch::Amd64).is_ok());

    let windows_10 = Table {
        present: Ok(true),
        reset: Ok(false),
    };
    assert_eq!(guest_reset(&windows_10, Arch::Amd64).unwrap_err().missing, RESET_TEXT);
}

#[skuld::test]
fn arm64_resets_without_the_hypervisor() {
    let windows_10 = Table {
        present: Ok(true),
        reset: Ok(false),
    };
    assert!(guest_reset(&windows_10, Arch::Arm64).is_ok());
}

#[skuld::test]
fn a_platform_that_cannot_be_asked_is_unavailable_with_its_reason() {
    let unloadable = Table {
        present: Ok(true),
        reset: Err("cannot load WinHvPlatform.dll: 126"),
    };
    let missing = guest_reset(&unloadable, Arch::Amd64).unwrap_err().missing;
    assert!(missing.contains("cannot load WinHvPlatform.dll: 126"), "{missing}");
    assert!(missing.contains("Windows Hypervisor Platform"), "{missing}");
}
