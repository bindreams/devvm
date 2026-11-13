//! Guest paths of a Windows guest as devvm's SFTP client spells them (measured): the server takes `C:\x`, `C:/x`,
//! `/C:/x`, `c:/x`, `\x` and a relative path for one file, answers `realpath` with the `/C:/Users/user` form, does not
//! expand `~`, and reads backslashes as separators, which devvm's own path planning does not.

use super::super::windows_root;
use crate::access::paths::expand_guest_path;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "paths_tests.rs"]
mod paths_tests;

/// The drive letter and the rest of `path` when it starts with a drive (`C:`, followed by `/` or nothing).
fn drive_of(path: &str) -> Option<(char, &str)> {
    let mut chars = path.chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    let rest = path.get(2..).filter(|_| chars.next() == Some(':'))?;
    (rest.is_empty() || rest.starts_with('/')).then_some((letter, rest))
}

/// `guest_path` as the server expects it: `~` expanded from `home` (the server does not), a drive as `/C:/`.
pub(super) fn sftp_path(guest_path: &str, home: &str) -> String {
    let expanded = expand_guest_path(guest_path, home);
    match drive_of(&expanded) {
        Some((letter, rest)) => format!("/{letter}:/{}", rest.trim_start_matches('/')),
        None => expanded,
    }
}

/// The path as the user would type it for a path the server reports: `/C:/x` is `C:/x`.
pub(super) fn user_path(sftp_path: &str) -> String {
    match sftp_path.strip_prefix('/') {
        Some(rest) if drive_of(rest).is_some() => rest.to_owned(),
        _ => sftp_path.to_owned(),
    }
}

/// The root of `path` and the rest, with `/C:/x` (the server's form of a drive path) read as `C:/x`. A backslash is
/// refused with the spelling that works: no Windows file name has one, so it is a separator typed out of habit, and
/// devvm splits paths at `/` only.
pub(super) fn split_root(path: &str) -> Result<(String, &str)> {
    if path.contains('\\') {
        return Err(Error::new(
            ErrorKind::Usage,
            format!(
                "the guest path {path} has a backslash, which devvm does not read as a separator; write {}",
                path.replace('\\', "/")
            ),
        ));
    }
    let drive_form = path
        .strip_prefix('/')
        .filter(|rest| drive_of(rest).is_some())
        .unwrap_or(path);
    windows_root(drive_form)
}
