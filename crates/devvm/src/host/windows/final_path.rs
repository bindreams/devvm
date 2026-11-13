//! The path of an open file that leads to that file and to nothing else: no link in it.

use std::ffi::OsString;
use std::fs::File;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;

use windows_sys::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, VOLUME_NAME_DOS};

use super::MAX_PLAIN_PATH_UNITS;

#[cfg(test)]
#[path = "final_path_tests.rs"]
mod final_path_tests;

/// The final path of the open file, written plain (`C:\dir\name`, `\\server\share\name`) where it is short enough:
/// every link (junction, symlink) and short name in the path it was opened by is resolved, and `std::fs` and QEMU read
/// the plain form as they read any path.
pub(super) fn of_handle(file: &File) -> std::io::Result<PathBuf> {
    let mut buffer = vec![0u16; 260];
    loop {
        // SAFETY: the handle is open, and the pointer and length describe `buffer`.
        let written = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                VOLUME_NAME_DOS,
            )
        } as usize;
        match written {
            0 => return Err(std::io::Error::last_os_error()),
            // The length without the NUL, when it fits; else the length the call needs, with the NUL.
            n if n < buffer.len() => {
                buffer.truncate(n);
                return Ok(PathBuf::from(OsString::from_wide(&plain(buffer))));
            }
            n => buffer.resize(n, 0),
        }
    }
}

/// `path` without the `\\?\` that `GetFinalPathNameByHandleW` puts before every path, unless the plain form is a longer
/// path than a program that does not know long paths takes (or is no disk or share path, which has no plain form).
fn plain(path: Vec<u16>) -> Vec<u16> {
    let units = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
    let unc = units(r"\\?\UNC\");
    let verbatim = units(r"\\?\");
    let plain = if let Some(rest) = path.strip_prefix(unc.as_slice()) {
        units(r"\\").into_iter().chain(rest.iter().copied()).collect()
    } else if let Some(rest) = path.strip_prefix(verbatim.as_slice())
        && is_disk_root(rest)
    {
        rest.to_vec()
    } else {
        return path;
    };
    if plain.len() > MAX_PLAIN_PATH_UNITS {
        path
    } else {
        plain
    }
}

/// Whether `path` starts with a drive letter, a colon and a backslash.
fn is_disk_root(path: &[u16]) -> bool {
    matches!(path, [letter, colon, slash, ..]
        if char::from_u32(u32::from(*letter)).is_some_and(|c| c.is_ascii_alphabetic())
            && *colon == u16::from(b':')
            && *slash == u16::from(b'\\'))
}
