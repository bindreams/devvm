//! The host for operating systems devvm 0.1.0 does not run VMs on (Linux, macOS).

use std::net::Shutdown;
use std::path::Path;

use super::{Accel, Host, SystemDirs, Unavailable, create_new_with};
use crate::backend::qemu::uds::UnixStream;
use crate::exit::ExitWidth;
use crate::{Arch, Result};

#[cfg(unix)]
#[path = "unsupported/perm_unix.rs"]
mod perm;
#[cfg(not(unix))]
#[path = "unsupported/perm_other.rs"]
mod perm;

#[cfg(unix)]
#[path = "unsupported/socket_unix.rs"]
mod socket;
#[cfg(not(unix))]
#[path = "unsupported/socket_other.rs"]
mod socket;

pub(super) struct UnsupportedHost;

impl Host for UnsupportedHost {
    fn accelerator(&self, _arch: Arch) -> std::result::Result<Accel, Unavailable> {
        Err(Unavailable {
            missing: format!("{} hosts are not supported in devvm 0.1.0", std::env::consts::OS),
        })
    }

    fn guest_reset(&self, _arch: Arch) -> std::result::Result<(), Unavailable> {
        Ok(())
    }

    fn socket_path_max(&self) -> usize {
        socket::path_max()
    }

    fn interrupt_connection(&self, connection: &UnixStream) -> std::io::Result<()> {
        // A thread blocked reading wakes with the end of the stream. Linux delivers the bytes already received first;
        // macOS drops them (measured in CI). A peer that is
        // gone already leaves nothing to wake: its end of the stream has been delivered.
        match connection.shutdown(Shutdown::Read) {
            Err(e) if e.kind() == std::io::ErrorKind::NotConnected => Ok(()),
            outcome => outcome,
        }
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
}
