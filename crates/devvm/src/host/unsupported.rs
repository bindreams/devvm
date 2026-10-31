//! What every host devvm 0.1.0 does not run VMs on (Linux, macOS) shares. A host type supplies only what differs, the
//! file modes ([`FileModes`]), and is a [`Host`] through the blanket impl below.

use std::fs::OpenOptions;
use std::path::Path;

use super::names::posix_problem;
use super::{Accel, Host, SystemDirs, Unavailable, after_failed_write, create_new_with};
use crate::exit::ExitWidth;
use crate::{Arch, Result};

/// How a host that cannot run VMs treats file modes and symlinks.
pub(super) trait FileModes: Send + Sync {
    /// Options that create a file readable and writable by its owner only, from the first moment it exists.
    fn private_options(&self) -> OpenOptions;

    fn is_executable(&self, path: &Path) -> std::io::Result<bool>;

    fn set_executable(&self, path: &Path) -> std::io::Result<()>;

    fn read_link(&self, path: &Path) -> std::io::Result<String>;

    fn make_symlink(&self, target: &str, link: &Path, to_dir: bool) -> std::io::Result<()>;

    fn sync_dir(&self, dir: &Path) -> std::io::Result<()>;
}

impl<M: FileModes> Host for M {
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
        create_new_with(self.private_options(), path, bytes)
    }

    fn is_executable(&self, path: &Path) -> std::io::Result<bool> {
        FileModes::is_executable(self, path)
    }

    fn set_executable(&self, path: &Path) -> std::io::Result<()> {
        FileModes::set_executable(self, path)
    }

    fn file_name_problem(&self, name: &str) -> Option<String> {
        posix_problem(name)
    }

    fn name_key(&self, name: &str) -> String {
        name.to_owned()
    }

    fn replace_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::rename(from, to)
    }

    /// Claims `to` with `create_dir`, which fails for anything that exists, and renames over the claim: a rename of a
    /// directory replaces an empty one atomically, so `from` is there in full or not at all.
    fn publish_dir(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::create_dir(to)?;
        // The claim is empty, since only this call has it.
        std::fs::rename(from, to).map_err(|e| after_failed_write(to, e, || std::fs::remove_dir(to)))
    }

    fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        FileModes::sync_dir(self, dir)
    }

    fn read_link(&self, path: &Path) -> std::io::Result<String> {
        FileModes::read_link(self, path)
    }

    fn make_symlink(&self, target: &str, link: &Path, to_dir: bool) -> std::io::Result<()> {
        FileModes::make_symlink(self, target, link, to_dir)
    }
}
