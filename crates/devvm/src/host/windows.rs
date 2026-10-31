//! The Windows host.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use cosca::identity::ProcessId;

use windows_sys::Win32::Globalization::GetACP;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES, MoveFileExW};
use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use super::names::{windows_key, windows_problem};
use super::{Accel, Host, ProcessWatch, SystemDirs, Unavailable, create_new_with};
use crate::exit::ExitWidth;
use crate::{Arch, Result};

mod link;
mod watch;
mod whpx;

#[cfg(test)]
#[path = "windows_tests.rs"]
mod windows_tests;

pub(crate) use watch::WindowsWatch;

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

/// `first`, which stays the error's kind, and the `second` failure that came while dealing with it.
pub(super) fn also_failed(first: std::io::Error, then: &str, second: std::io::Error) -> std::io::Error {
    std::io::Error::new(first.kind(), format!("{first}; {then} failed as well ({second})"))
}

pub(super) struct WindowsHost;

impl Host for WindowsHost {
    fn accelerator(&self, arch: Arch) -> std::result::Result<Accel, Unavailable> {
        whpx::accelerator(&whpx::WinHvPlatform, arch)
    }

    fn guest_reset(&self, arch: Arch) -> std::result::Result<(), Unavailable> {
        whpx::guest_reset(&whpx::WinHvPlatform, arch)
    }

    /// `sun_path` is 108 bytes.
    fn socket_path_max(&self) -> usize {
        107
    }

    fn watch_process(&self, id: ProcessId) -> Result<Arc<dyn ProcessWatch>> {
        Ok(Arc::new(WindowsWatch::open(id)?))
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

    /// Opened for `FILE_WRITE_ATTRIBUTES` alone: the read-only attribute, or an ACL that denies writing the contents,
    /// does not stop it. A directory opens only with backup semantics.
    fn set_mtime(&self, path: &Path, seconds: u32) -> std::io::Result<()> {
        OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?
            .set_modified(UNIX_EPOCH + Duration::from_secs(u64::from(seconds)))
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
        let denied = match std::fs::rename(from, to) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => e,
            other => return other,
        };
        let metadata = match std::fs::symlink_metadata(to) {
            Ok(metadata) => metadata,
            Err(second) => return Err(also_failed(denied, "looking at the target", second)),
        };
        let mut permissions = metadata.permissions();
        if metadata.file_type().is_symlink() || !permissions.readonly() {
            return Err(denied);
        }
        permissions.set_readonly(false);
        if let Err(second) = std::fs::set_permissions(to, permissions.clone()) {
            return Err(also_failed(denied, "clearing the read-only attribute", second));
        }
        std::fs::rename(from, to).map_err(|first| {
            permissions.set_readonly(true);
            match std::fs::set_permissions(to, permissions) {
                Ok(()) => first,
                Err(second) => also_failed(first, "putting the read-only attribute back", second),
            }
        })
    }

    /// `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` fails with `ERROR_ALREADY_EXISTS` for anything at `to`.
    fn publish_dir(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        let wide = |path: &Path| {
            path.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>()
        };
        let (from, to) = (wide(from), wide(to));
        // SAFETY: both are NUL-terminated and outlive the call.
        let moved = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
        if moved == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// NTFS journals directory changes, and a directory cannot be flushed through a handle anyway.
    fn sync_dir(&self, _dir: &Path) -> std::io::Result<()> {
        Ok(())
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

    fn symlinks_have_kinds(&self) -> bool {
        true
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
