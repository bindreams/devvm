//! The Windows host.

use std::ffi::{OsStr, OsString};
use std::fs::{File, FileType, OpenOptions};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use cosca::identity::ProcessId;

use windows_sys::Win32::Foundation::ERROR_PRIVILEGE_NOT_HELD;
use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal, GetACP};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES, MoveFileExW};
use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use super::Hold;
use super::entry;
use super::names::{windows_colon_path, windows_problem, windows_run_dir};
use super::{Accel, Entry, FileId, HoldOutcome, Host, ProcessWatch, SystemDirs, Unavailable, create_new_with};
use crate::exit::ExitWidth;
use crate::{Arch, Error, ErrorKind, Result};

mod file_id;
mod hold;
mod link;
pub(crate) mod runtime_dir;
pub(crate) mod user;
mod volume;
mod watch;
mod whpx;

#[cfg(test)]
#[path = "windows_tests.rs"]
mod windows_tests;

pub(crate) use watch::WindowsWatch;

/// The most UTF-16 units a path has and stays plain: `std`'s `get_long_path` returns a path unprefixed only while its
/// units and the NUL are fewer than its `LEGACY_MAX_PATH`, 248.
const MAX_PLAIN_PATH_UNITS: usize = 246;

/// `path` as the NUL-terminated UTF-16 a Win32 call takes: the one conversion for every call in this host that takes a
/// path. An interior NUL is `InvalidInput`, since the call would read the path only as far as it. The path is made
/// absolute. It gets the `\\?\` prefix (`\\?\UNC\` for a share) only when it is too long for the plain form, with
/// `std`'s threshold, so a short path is read by Win32's name rules as `std::fs` reads it. A verbatim or a device
/// path stays as it is.
pub(crate) fn wide(path: &Path) -> std::io::Result<Vec<u16>> {
    if path.as_os_str().encode_wide().any(|unit| unit == 0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file name contained an unexpected NUL byte",
        ));
    }
    let absolute = std::path::absolute(path)?;
    let units: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    let (head, rest): (&str, &[u16]) = match absolute.components().next() {
        Some(Component::Prefix(prefix)) if units.len() > MAX_PLAIN_PATH_UNITS => match prefix.kind() {
            Prefix::Disk(_) => (r"\\?\", &units),
            // `\\server\share` becomes `\\?\UNC\server\share`.
            Prefix::UNC(..) => (r"\\?\UNC\", &units[2..]),
            _ => ("", &units),
        },
        _ => ("", &units),
    };
    Ok(head.encode_utf16().chain(rest.iter().copied()).chain(Some(0)).collect())
}

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

    fn run_dir_default(&self, _var: &dyn Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
        let cannot = |what: &str, source| {
            Error::io(
                ErrorKind::Config,
                format!("cannot locate the runtime directory: {what}"),
                source,
            )
        };
        let program_data = user::program_data().map_err(|e| cannot("the ProgramData folder is unknown", e))?;
        let sid = user::current_sid().map_err(|e| cannot("the process token's user is unknown", e))?;
        Ok(windows_run_dir(&program_data, &sid))
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

    /// The first `prefix.len()` UTF-16 units are compared by the OS, one for one: an ASCII prefix unit only ever
    /// matches a single unit, so the rest starts on a unit boundary.
    fn env_name_strip_prefix(&self, name: &OsStr, prefix: &str) -> Option<String> {
        debug_assert!(prefix.is_ascii());
        let name: Vec<u16> = name.encode_wide().collect();
        let prefix: Vec<u16> = prefix.encode_utf16().collect();
        let head = name.get(..prefix.len())?;
        let len = i32::try_from(prefix.len()).expect("a prefix is a few units long");
        // SAFETY: both pointers describe live slices of `len` units.
        let ordering = unsafe { CompareStringOrdinal(head.as_ptr(), len, prefix.as_ptr(), len, 1) };
        // The call fails, with 0, only for a bad pointer, length or flag, and none of them is bad here. Read as "not
        // equal", it would let a `GIT_*` name through.
        assert_ne!(ordering, 0, "CompareStringOrdinal rejected two valid slices");
        (ordering == CSTR_EQUAL).then(|| String::from_utf16_lossy(&name[prefix.len()..]))
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

    fn cannot_hold_path(&self, rel: &str) -> bool {
        windows_colon_path(rel)
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
        let (from, to) = (wide(from)?, wide(to)?);
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
            Some(code) if code == ERROR_PRIVILEGE_NOT_HELD as i32 => std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Windows lets only Developer Mode or an elevated token create symlinks",
            ),
            _ => e,
        })
    }

    fn create_runtime_dir(&self, dir: &Path) -> std::io::Result<()> {
        runtime_dir::create(dir)
    }

    fn accept_runtime_dir(&self, dir: &Path) -> std::io::Result<()> {
        runtime_dir::accept(dir)
    }

    fn file_id(&self, path: &Path) -> std::io::Result<FileId> {
        file_id::of(path)
    }

    fn file_id_of_open(&self, file: &File) -> std::io::Result<FileId> {
        file_id::of_handle(file)
    }

    fn volume_has_posix_unlink_rename(&self, file: &File) -> std::io::Result<bool> {
        volume::has_posix_unlink_rename(file)
    }

    fn hold_new(&self, path: &Path) -> std::io::Result<Box<dyn Hold>> {
        Ok(Box::new(hold::new(path)?))
    }

    fn open_lock_file(&self, path: &Path, create: bool) -> std::io::Result<File> {
        hold::open_lock_file(path, create)
    }

    fn hold_existing(&self, path: &Path) -> std::io::Result<HoldOutcome> {
        hold::existing(path)
    }

    fn remove_deleting_name(&self, path: &Path) -> std::io::Result<()> {
        hold::remove_deleting_name(path)
    }

    fn open_entry(&self, root: &Path, rel: &str) -> std::io::Result<Entry> {
        // `name:stream` reads an alternate data stream of the file `name`, which is no file git tracks.
        if windows_colon_path(rel) {
            return Ok(Entry::Absent);
        }
        entry::open_entry(
            root,
            rel,
            |path| File::open(path),
            |_: FileType| "of a type this host does not know",
        )
    }
}
