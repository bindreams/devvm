//! The Windows host.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Globalization::GetACP;
use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use super::names::{windows_key, windows_problem};
use super::{Accel, Host, SystemDirs, Unavailable, create_new_with};
use crate::exit::ExitWidth;
use crate::{Arch, Result};

/// `ERROR_PRIVILEGE_NOT_HELD`: the user may not create symlinks.
const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

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

    fn file_name_problem(&self, name: &str) -> Option<String> {
        windows_problem(name)
    }

    fn name_key(&self, name: &str) -> String {
        windows_key(name)
    }

    /// Renaming over a read-only file fails with access denied, so the flag is cleared first and put back if the rename
    /// still fails.
    #[allow(
        clippy::permissions_set_readonly_false,
        reason = "on Windows the flag is the read-only attribute, not Unix mode bits"
    )]
    fn replace_file(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        match std::fs::rename(from, to) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let metadata = std::fs::symlink_metadata(to)?;
                let mut permissions = metadata.permissions();
                if metadata.file_type().is_symlink() || !permissions.readonly() {
                    return Err(e);
                }
                permissions.set_readonly(false);
                std::fs::set_permissions(to, permissions.clone())?;
                std::fs::rename(from, to).inspect_err(|_| {
                    permissions.set_readonly(true);
                    let _ = std::fs::set_permissions(to, permissions);
                })
            }
            other => other,
        }
    }

    fn read_link(&self, path: &Path) -> std::io::Result<String> {
        let target = std::fs::read_link(path)?;
        match target.to_str() {
            Some(text) => Ok(text.replace('\\', "/")),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("the symlink target {target:?} is not valid Unicode"),
            )),
        }
    }

    /// A target written with `/` is not followed by Windows, so it is stored with `\`.
    fn make_symlink(&self, target: &str, link: &Path, to_dir: bool) -> std::io::Result<()> {
        let target = target.replace('/', "\\");
        let made = if to_dir {
            std::os::windows::fs::symlink_dir(&target, link)
        } else {
            std::os::windows::fs::symlink_file(&target, link)
        };
        made.map_err(|e| match e.raw_os_error() {
            Some(ERROR_PRIVILEGE_NOT_HELD) => std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Windows lets only Developer Mode or an elevated token create symlinks",
            ),
            _ => e,
        })
    }
}
