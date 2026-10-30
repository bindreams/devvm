//! The Windows host.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Globalization::GetACP;
use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use super::{Accel, Host, SystemDirs, Unavailable, create_new_with};
use crate::exit::ExitWidth;
use crate::{Arch, Result};

/// A directory path from a `Get*DirectoryW` call, which returns the length it needs when the buffer is too small.
fn directory(get: unsafe extern "system" fn(*mut u16, u32) -> u32) -> Option<PathBuf> {
    let mut buffer = vec![0u16; 260];
    loop {
        // SAFETY: the pointer and length describe `buffer`.
        let written = unsafe { get(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
        match written {
            0 => return None,
            n if n < buffer.len() => return Some(PathBuf::from(OsString::from_wide(&buffer[..n]))),
            n => buffer.resize(n + 1, 0),
        }
    }
}

pub(super) struct WindowsHost;

impl Host for WindowsHost {
    fn accelerator(&self, _arch: Arch) -> std::result::Result<Accel, Unavailable> {
        Err(Unavailable {
            missing: "WHPX detection is not implemented".to_owned(),
        })
    }

    fn ansi_code_page(&self) -> Option<u32> {
        // SAFETY: `GetACP` takes no arguments and cannot fail.
        Some(unsafe { GetACP() })
    }

    /// `DETACHED_PROCESS`: the VM has no console, so no console control event can reach it.
    fn detach(&self, cmd: &mut cosca::Command) -> Result<()> {
        cmd.detached();
        Ok(())
    }

    fn exit_width(&self) -> ExitWidth {
        ExitWidth::Full32
    }

    fn quiet(&self, cmd: &mut std::process::Command) {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    fn system_dirs(&self) -> Option<SystemDirs> {
        Some(SystemDirs {
            root: directory(GetWindowsDirectoryW)?,
            system32: directory(GetSystemDirectoryW)?,
        })
    }

    /// The file keeps the ACL it inherits from its directory, which OpenSSH for Windows accepts.
    fn write_private(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        create_new_with(OpenOptions::new(), path, bytes)
    }

    fn is_executable(&self, _path: &Path) -> std::io::Result<bool> {
        Ok(false)
    }

    fn set_executable(&self, _path: &Path) -> std::io::Result<()> {
        Ok(())
    }
}
