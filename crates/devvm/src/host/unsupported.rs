//! The host for operating systems devvm 0.1.0 does not run VMs on (Linux, macOS).

use std::path::Path;

use super::names::posix_problem;
use super::{Accel, Host, SystemDirs, Unavailable, create_new_with};
use crate::exit::ExitWidth;
use crate::{Arch, Result};

#[cfg(unix)]
#[path = "unsupported/perm_unix.rs"]
mod perm;
#[cfg(not(unix))]
#[path = "unsupported/perm_other.rs"]
mod perm;

pub(super) struct UnsupportedHost;

impl Host for UnsupportedHost {
    fn accelerator(&self, _arch: Arch) -> std::result::Result<Accel, Unavailable> {
        Err(Unavailable {
            missing: format!("{} hosts are not supported in devvm 0.1.0", std::env::consts::OS),
        })
    }

    fn ansi_code_page(&self) -> Option<u32> {
        None
    }

    /// A plain spawn already outlives its parent; the host is refused earlier by `accelerator`.
    fn detach(&self, _cmd: &mut cosca::Command) -> Result<()> {
        Ok(())
    }

    fn exit_width(&self) -> ExitWidth {
        ExitWidth::LowByte
    }

    fn quiet(&self, _cmd: &mut std::process::Command) {}

    fn system_dirs(&self) -> Option<SystemDirs> {
        None
    }

    /// Mode `0600` is set when the file is created, so there is no moment with wider permissions.
    fn write_private(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        create_new_with(perm::private_options(), path, bytes)
    }

    fn is_executable(&self, path: &Path) -> std::io::Result<bool> {
        perm::is_executable(path)
    }

    fn set_executable(&self, path: &Path) -> std::io::Result<()> {
        perm::set_executable(path)
    }

    fn file_name_problem(&self, name: &str) -> Option<String> {
        posix_problem(name)
    }

    fn name_key(&self, name: &str) -> String {
        name.to_owned()
    }
}
