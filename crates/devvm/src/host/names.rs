//! Which file names a host's file system can hold. The rules are plain functions so the tests can apply a Windows
//! host's rules on any OS.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

#[cfg(test)]
#[path = "names_tests.rs"]
mod names_tests;

/// What no host can hold as one file name.
fn common_problem(name: &str) -> Option<String> {
    if name.is_empty() || name == "." || name == ".." {
        return Some("is not a file name".to_owned());
    }
    if name.contains('/') || name.contains('\0') {
        return Some("contains a `/` or a NUL".to_owned());
    }
    None
}

/// The longest a file name can be: in bytes of UTF-8 on Unix, in UTF-16 units on NTFS.
const NAME_MAX: usize = 255;

/// The rules of a Unix file system: any bytes except `/` and NUL, at most 255 of them.
#[cfg_attr(windows, allow(dead_code, reason = "used by the host this binary does not run on"))]
pub(crate) fn posix_problem(name: &str) -> Option<String> {
    if let Some(problem) = common_problem(name) {
        return Some(problem);
    }
    if name.len() > NAME_MAX {
        return Some(format!(
            "is {} bytes, over the {NAME_MAX} bytes a file name can have",
            name.len()
        ));
    }
    None
}

/// The rules of NTFS and Win32: no `\ : < > " | ? *`, no characters below U+0020, at most 255 UTF-16 units. Trailing
/// dots and spaces, device names and names that differ only in case are not checked: `devvm cp` takes trees without
/// them.
pub(crate) fn windows_problem(name: &str) -> Option<String> {
    if let Some(problem) = common_problem(name) {
        return Some(problem);
    }
    let units = name.encode_utf16().count();
    if units > NAME_MAX {
        return Some(format!(
            "is {units} UTF-16 units, over the {NAME_MAX} UTF-16 units a Windows file name can have"
        ));
    }
    if let Some(c) = name
        .chars()
        .find(|c| matches!(c, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return Some(format!("contains `{c}`, which Windows file names cannot have"));
    }
    // NTFS holds DEL and the C1 controls.
    if name.chars().any(|c| ('\u{1}'..='\u{1f}').contains(&c)) {
        return Some("contains a control character, which Windows file names cannot have".to_owned());
    }
    None
}

/// Whether a segment of `path` (`/`-separated) has a `:`, which NTFS and Win32 read as a data stream (`name:stream`)
/// or a drive (`d:x`), never as part of a file name. Decided from the text alone: asking the file system about such a
/// name can fail (`d::$DATA` below a directory `d` is access denied) or reach another file.
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "used by the host this binary does not run on")
)]
pub(crate) fn windows_colon_path(path: &str) -> bool {
    path.split('/').any(|segment| segment.contains(':'))
}

/// The runtime directory of a Unix host without `XDG_RUNTIME_DIR`: `<TMPDIR or /tmp>/devvm-run-<euid>`. `/tmp` is
/// shared by every user, so the directory is per user. An empty or relative `TMPDIR` is ignored.
#[cfg_attr(windows, allow(dead_code, reason = "used by the host this binary does not run on"))]
pub(crate) fn unix_run_dir(tmpdir: Option<OsString>, euid: u32) -> PathBuf {
    let base = tmpdir
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(format!("devvm-run-{euid}"))
}

/// The runtime directory of a Windows user: `<program_data>\devvm-run-<16 hex>`, the hex being the first 8 bytes of
/// SHA-256 over the SID string. Every account gets a name of one length, so the socket path under it never depends on
/// how long the SID is (a service SID has 63 characters, a user's 52).
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "used by the host this binary does not run on")
)]
pub(crate) fn windows_run_dir(program_data: &Path, sid: &str) -> PathBuf {
    let digest = Sha256::digest(sid.as_bytes());
    program_data.join(format!("devvm-run-{}", hex::encode(&digest[..8])))
}
