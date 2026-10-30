//! The host side of a copy: reading a source tree, and writing a destination. It runs on the calling thread; every
//! platform difference goes through [`Host`].

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::plan::{Beyond, Item, Kind, Present};
use super::{local_error, partial_name};
use crate::host::Host;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "local_tests.rs"]
mod local_tests;

/// Whether `path` is written with a trailing separator, as `dir/`.
pub(super) fn ends_with_separator(path: &Path) -> bool {
    path.as_os_str().to_string_lossy().ends_with(std::path::is_separator)
}

/// `path` without trailing separators. A root is left as it is: `C:` would name the current directory of the drive, and
/// `\?\C:` the volume itself.
pub(super) fn without_trailing_separator(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    let trimmed = text.trim_end_matches(std::path::is_separator);
    let is_root =
        trimmed.is_empty() || matches!(Path::new(trimmed).components().next_back(), Some(Component::Prefix(_)));
    if is_root || trimmed.len() == text.len() {
        path.to_owned()
    } else {
        PathBuf::from(trimmed)
    }
}

/// `path` with its `.` segments dropped and each `..` cancelled against the name before it, by name only: a symlink
/// before a `..` is not followed. A `..` at a root stays at the root; one with nothing before it stays; no segment left
/// is `.`. This is how a destination like `new/.` or `a/../b` is created exactly as it reads, before any directory is
/// made.
pub(super) fn lexically_normal(path: &Path) -> PathBuf {
    let mut parts: Vec<Component> = Vec::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => match parts.last() {
                Some(Component::Normal(_)) => {
                    parts.pop();
                }
                Some(Component::RootDir) => {}
                _ => parts.push(part),
            },
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return PathBuf::from(".");
    }
    // Spelled out, not pushed: `PathBuf::push` reads a name like `a:b` as a drive on Windows and drops what came before.
    let mut text = OsString::new();
    for (i, part) in parts.iter().enumerate() {
        if matches!(
            i.checked_sub(1).map(|before| parts[before]),
            Some(Component::Normal(_) | Component::ParentDir)
        ) {
            text.push(std::path::MAIN_SEPARATOR_STR);
        }
        text.push(part.as_os_str());
    }
    PathBuf::from(text)
}

/// The parts of the host path `path` from the outermost, for [`super::plan::check_destination`]: each with the path
/// that ends in it.
pub(super) fn chain(path: &Path) -> Vec<(Option<String>, PathBuf)> {
    let mut so_far = PathBuf::new();
    path.components()
        .map(|component| {
            so_far.push(component.as_os_str());
            let name = match component {
                Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                _ => None,
            };
            (name, so_far.clone())
        })
        .collect()
}

/// Seconds since 1970 as SFTP carries them; `None` for a time before 1970 or after 2106, which is then not kept.
fn sftp_seconds(time: std::io::Result<SystemTime>) -> Option<u32> {
    let since = time.ok()?.duration_since(UNIX_EPOCH).ok()?;
    u32::try_from(since.as_secs()).ok()
}

// Reading =============================================================================================================

/// The tree entry that `metadata` describes at `path`, or `None` for a directory's contents to be walked.
fn kind_of(path: &Path, metadata: &std::fs::Metadata, host: &dyn Host) -> Result<Kind> {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = host
            .read_link(path)
            .map_err(|e| local_error("cannot read the symlink", path, e))?;
        Ok(Kind::Link { target, to_dir: false })
    } else if file_type.is_dir() {
        Ok(Kind::Dir {
            mtime: sftp_seconds(metadata.modified()),
        })
    } else if file_type.is_file() {
        let executable = host
            .is_executable(path)
            .map_err(|e| local_error("cannot read the mode of", path, e))?;
        Ok(Kind::File {
            executable,
            mtime: sftp_seconds(metadata.modified()),
            size: metadata.len(),
        })
    } else {
        Err(Error::new(
            ErrorKind::Io,
            format!(
                "{} is neither a regular file, a directory nor a symlink",
                path.display()
            ),
        ))
    }
}

/// The source `path`: a symlink is a link unless the path ends in a separator, which follows it once.
pub(super) fn inspect(path: &Path, host: &dyn Host) -> Result<Kind> {
    let metadata = if ends_with_separator(path) {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    }
    .map_err(|e| local_error("the host source", path, e))?;
    kind_of(path, &metadata, host)
}

/// The host tree below `dir`, parents before children, symlinks as links.
pub(super) fn walk(dir: &Path, host: &dyn Host) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    let mut pending = vec![(dir.to_owned(), Vec::<String>::new())];
    while let Some((path, rel)) = pending.pop() {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&path).map_err(|e| local_error("cannot list", &path, e))? {
            let entry = entry.map_err(|e| local_error("cannot list", &path, e))?;
            names.push(entry.file_name());
        }
        names.sort();
        let mut dirs = Vec::new();
        for name in names {
            let child = path.join(&name);
            let Some(text) = name.to_str() else {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!(
                        "{} has a name that is not valid Unicode, which SFTP cannot carry",
                        child.display()
                    ),
                ));
            };
            let metadata = std::fs::symlink_metadata(&child).map_err(|e| local_error("cannot read", &child, e))?;
            let kind = kind_of(&child, &metadata, host)?;
            let mut child_rel = rel.clone();
            child_rel.push(text.to_owned());
            if kind.is_dir() {
                dirs.push((child, child_rel.clone()));
            }
            items.push(Item { rel: child_rel, kind });
        }
        // Pushed in reverse so the first directory is walked first.
        pending.extend(dirs.into_iter().rev());
    }
    Ok(items)
}

// Writing =============================================================================================================

/// What the destination `path` is. A symlink is reported as leading to a directory only when `follow` is set, which
/// looks where it leads (and fails when that cannot be told); without it, every link is `Link(Beyond::NotDir)`, for a
/// link that the copy replaces whatever is behind it.
pub(super) fn present(path: &Path, follow: bool) -> Result<Present> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(Present::Link(if follow && leads_to_dir(path)? {
            Beyond::Dir
        } else {
            Beyond::NotDir
        })),
        Ok(metadata) if metadata.is_dir() => Ok(Present::Dir),
        Ok(_) => Ok(Present::File),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Present::Nothing),
        Err(e) => Err(local_error("cannot inspect", path, e)),
    }
}

/// Whether the symlink `path` leads to a directory. A dangling link leads to none; a target that cannot be looked at (a
/// loop, no permission) is an error.
fn leads_to_dir(path: &Path) -> Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(local_error("cannot inspect where the symlink leads at", path, e)),
    }
}

pub(super) fn make_dirs(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|e| local_error("cannot create the directory", path, e))
}

/// Makes `path` a directory for the tree entry `what`: a missing one is made with its parents, a symlink is replaced
/// by a directory (or followed, when `follow`), a file is an error. Returns whether it made the directory.
pub(super) fn put_dir(path: &Path, follow: bool, what: &str) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => make_dirs(path).map(|()| true),
        Err(e) => Err(local_error("cannot inspect", path, e)),
        Ok(metadata) if metadata.is_dir() => Ok(false),
        Ok(metadata) if metadata.file_type().is_symlink() && follow && leads_to_dir(path)? => Ok(false),
        Ok(metadata) if metadata.file_type().is_symlink() && !follow => {
            // A symlink to a directory is removed as a directory on Windows and as a file elsewhere.
            std::fs::remove_file(path)
                .or_else(|_| std::fs::remove_dir(path))
                .map_err(|e| local_error("cannot replace the symlink", path, e))?;
            make_dirs(path).map(|()| true)
        }
        Ok(_) => Err(Error::new(
            ErrorKind::Io,
            format!(
                "cannot copy {what} onto the host path {}, which is not a directory",
                path.display()
            ),
        )),
    }
}

/// The temporary name for a file that will replace `target`: beside it, so the rename stays on one file system.
pub(super) fn temp_beside(target: &Path) -> PathBuf {
    match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(partial_name()),
        _ => PathBuf::from(partial_name()),
    }
}

/// Moves the temporary `temp` over `target`, replacing a file or a symlink there, and removes `temp` when it fails.
fn replace(host: &dyn Host, temp: &Path, target: &Path) -> Result<()> {
    host.replace_file(temp, target).map_err(|e| {
        let _ = std::fs::remove_file(temp);
        local_error("cannot move the copy into place at", target, e)
    })
}

/// Makes the downloaded `temp` the file `target`, with the modification time and execute bit of the guest file.
pub(super) fn finish_file(
    temp: &Path,
    target: &Path,
    executable: bool,
    mtime: Option<u32>,
    host: &dyn Host,
) -> Result<()> {
    if let Some(seconds) = mtime {
        let time = UNIX_EPOCH + Duration::from_secs(u64::from(seconds));
        std::fs::OpenOptions::new()
            .write(true)
            .open(temp)
            .and_then(|file| file.set_modified(time))
            .map_err(|e| {
                let _ = std::fs::remove_file(temp);
                local_error("cannot set the time of", temp, e)
            })?;
    }
    replace(host, temp, target)?;
    if executable {
        host.set_executable(target)
            .map_err(|e| local_error("cannot make executable", target, e))?;
    }
    Ok(())
}

/// Whether this host can make symlinks where a copy to `dest` would: `None` when it can, else what it said. The probe
/// is a link made and removed in the nearest directory that exists.
pub(super) fn probe_links(host: &dyn Host, dest: &Path) -> Result<Option<String>> {
    let dir = dest
        .ancestors()
        .map(|path| {
            if path.as_os_str().is_empty() {
                Path::new(".")
            } else {
                path
            }
        })
        .find(|path| path.is_dir())
        .ok_or_else(|| Error::new(ErrorKind::Io, format!("no directory above {} exists", dest.display())))?;
    let probe = dir.join(partial_name());
    if let Err(e) = host.make_symlink(".", &probe, false) {
        return Ok(Some(e.to_string()));
    }
    std::fs::remove_file(&probe).map_err(|e| local_error("cannot remove the test symlink", &probe, e))?;
    Ok(None)
}

/// Sets the modification time of the directory `path`, after its children are written.
pub(super) fn set_dir_time(path: &Path, seconds: u32) -> Result<()> {
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(i64::from(seconds), 0))
        .map_err(|e| local_error("cannot set the time of", path, e))
}

/// Makes `target` a symlink holding `text` (a guest path, `/`-separated), in place of whatever file or symlink is
/// there. `what` names the guest entry.
pub(super) fn put_link(host: &dyn Host, text: &str, to_dir: bool, target: &Path, what: &str) -> Result<()> {
    let temp = temp_beside(target);
    host.make_symlink(text, &temp, to_dir).map_err(|e| {
        Error::io(
            ErrorKind::Io,
            format!("cannot copy {what} to {} on this host: {e}", target.display()),
            e,
        )
    })?;
    replace(host, &temp, target)
}
