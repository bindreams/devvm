//! The Windows Hypervisor Platform (WHPX): whether it runs, and whether a running guest can be reset.
//!
//! `WinHvPlatform.dll` is loaded from System32 only, as QEMU does, and only when asked: devvm starts on a machine
//! without the feature.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::FreeLibrary;
use windows_sys::Win32::System::Hypervisor::{WHV_CAPABILITY, WHvCapabilityCodeHypervisorPresent};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

use crate::Arch;
use crate::host::{Accel, Unavailable};

#[cfg(test)]
#[path = "whpx_tests.rs"]
mod whpx_tests;

/// What the platform DLL tells. An `Err` is why it could not be asked.
pub(super) trait Hypervisor {
    /// Whether the Windows hypervisor is running (`WHvGetCapability(WHvCapabilityCodeHypervisorPresent)`).
    fn present(&self) -> Result<bool, String>;

    /// Whether the DLL exports `WHvResetPartition` (Windows 11 and later).
    fn has_reset_partition(&self) -> Result<bool, String>;
}

/// The platform of this machine.
pub(super) struct WinHvPlatform;

type GetCapability = unsafe extern "system" fn(i32, *mut c_void, u32, *mut u32) -> i32;

/// Calls `use_dll` with the loaded `WinHvPlatform.dll`, and unloads it after.
fn with_dll<T>(use_dll: impl FnOnce(*mut c_void) -> Result<T, String>) -> Result<T, String> {
    let name: Vec<u16> = "WinHvPlatform.dll\0".encode_utf16().collect();
    // SAFETY: `name` is NUL-terminated; the flag restricts the search to System32.
    let dll = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if dll.is_null() {
        return Err(format!(
            "cannot load WinHvPlatform.dll from System32: {}",
            std::io::Error::last_os_error()
        ));
    }
    let result = use_dll(dll);
    // SAFETY: `dll` came from `LoadLibraryExW` just above and nothing uses it after this.
    unsafe { FreeLibrary(dll) };
    result
}

/// The address of the export `name` (NUL-terminated), if there is one.
fn export(dll: *mut c_void, name: &[u8]) -> Option<unsafe extern "system" fn() -> isize> {
    debug_assert_eq!(name.last(), Some(&0));
    // SAFETY: `dll` is a loaded module and `name` is NUL-terminated.
    unsafe { GetProcAddress(dll, name.as_ptr()) }
}

impl Hypervisor for WinHvPlatform {
    fn present(&self) -> Result<bool, String> {
        with_dll(|dll| {
            let Some(function) = export(dll, b"WHvGetCapability\0") else {
                return Err("WinHvPlatform.dll has no WHvGetCapability".to_owned());
            };
            // SAFETY: `WHvGetCapability` has this signature (winhvplatform.h).
            let get: GetCapability = unsafe { std::mem::transmute(function) };
            // SAFETY: an all-zero capability buffer is valid for every member of the union.
            let mut capability: WHV_CAPABILITY = unsafe { std::mem::zeroed() };
            let mut written = 0u32;
            // SAFETY: the pointer and size describe `capability`.
            let result = unsafe {
                get(
                    WHvCapabilityCodeHypervisorPresent,
                    (&mut capability as *mut WHV_CAPABILITY).cast(),
                    std::mem::size_of::<WHV_CAPABILITY>() as u32,
                    &mut written,
                )
            };
            if result < 0 {
                return Err(format!("WHvGetCapability failed with HRESULT {result:#010x}"));
            }
            // SAFETY: the call succeeded for the HypervisorPresent code, which fills this member.
            Ok(unsafe { capability.HypervisorPresent } != 0)
        })
    }

    fn has_reset_partition(&self) -> Result<bool, String> {
        with_dll(|dll| Ok(export(dll, b"WHvResetPartition\0").is_some()))
    }
}

fn feature_missing(why: &str) -> Unavailable {
    Unavailable {
        missing: format!(
            "the Windows Hypervisor Platform is not available ({why}); enable the optional feature with \
             `Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform` in an elevated PowerShell, \
             and restart"
        ),
    }
}

/// The accelerator for guests of `arch`: WHPX for amd64 when the hypervisor runs; software emulation for arm64, which
/// the hypervisor cannot run on this host.
pub(super) fn accelerator(hypervisor: &dyn Hypervisor, arch: Arch) -> Result<Accel, Unavailable> {
    match arch {
        Arch::Arm64 => Ok(Accel::Tcg),
        Arch::Amd64 => match hypervisor.present() {
            Ok(true) => Ok(Accel::Whpx),
            Ok(false) => Err(feature_missing("the hypervisor is not running")),
            Err(why) => Err(feature_missing(&why)),
        },
    }
}

/// Whether a running guest of `arch` can be reset inside its QEMU process.
pub(super) fn guest_reset(hypervisor: &dyn Hypervisor, arch: Arch) -> Result<(), Unavailable> {
    match arch {
        Arch::Arm64 => Ok(()),
        Arch::Amd64 => match hypervisor.has_reset_partition() {
            Ok(true) => Ok(()),
            Ok(false) => Err(Unavailable {
                missing: "rebooting a guest needs Windows 11 or later (this host's WinHvPlatform.dll has no \
                          WHvResetPartition)"
                    .to_owned(),
            }),
            Err(why) => Err(feature_missing(&why)),
        },
    }
}
