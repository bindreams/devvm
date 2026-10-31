//! The host side of a copy: reading a source tree, and writing a destination. It runs on the calling thread; every
//! platform difference goes through [`Host`].

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::copy::{CopySide, Receives};
use super::guest::GuestSide;
use super::plan::{Beyond, Item, Kind, Present};
use super::typed::Typed;
use super::{local_error, partial_name};
use crate::host::{Host, remove_if_present};
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
    // Spelled out, not pushed: `PathBuf::push` reads a name like `a:b` as a drive on Windows and drops what came
    // before.
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

/// Seconds since 1970 as SFTP carries them; `None` for a time before 1970 or after 2106, which is then not kept.
fn sftp_seconds(time: std::io::Result<SystemTime>) -> Option<u32> {
    let since = time.ok()?.duration_since(UNIX_EPOCH).ok()?;
    u32::try_from(since.as_secs()).ok()
}

/// The error of a symlink that could be removed neither as a file nor as a directory. The first failure is the cause on
/// every host but Windows, where it can be the second; both are said.
fn link_removal_failure(as_file: std::io::Error, as_dir: std::io::Error) -> std::io::Error {
    if as_file.to_string() == as_dir.to_string() {
        return as_file;
    }
    let text = format!("{as_file} (removing it as a directory link failed too: {as_dir})");
    std::io::Error::new(as_file.kind(), text)
}

// Reading =============================================================================================================

/// The tree entry that `metadata` describes at `path`, or `None` for a directory's contents to be walked.
fn kind_of(path: &Path, metadata: &std::fs::Metadata, side: &HostSide) -> Result<Kind> {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = side
            .host
            .read_link(path)
            .map_err(|e| local_error("cannot read the symlink", side.name(path), e))?;
        Ok(Kind::Link { target, to_dir: false })
    } else if file_type.is_dir() {
        Ok(Kind::Dir {
            mtime: sftp_seconds(metadata.modified()),
        })
    } else if file_type.is_file() {
        let executable = side
            .host
            .is_executable(path)
            .map_err(|e| local_error("cannot read the mode of", side.name(path), e))?;
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
                side.name(path)
            ),
        ))
    }
}

// Writing =============================================================================================================

/// Whether the symlink `path` leads to a directory. A dangling link leads to none; a target that cannot be looked at (a
/// loop, no permission) is an error.
fn leads_to_dir(path: &Path, side: &HostSide) -> Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(local_error(
            "cannot inspect where the symlink leads at",
            side.name(path),
            e,
        )),
    }
}

/// The temporary name for a file that will replace `target`: beside it, so the rename stays on one file system.
fn temp_beside(target: &Path) -> PathBuf {
    match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(partial_name()),
        _ => PathBuf::from(partial_name()),
    }
}

/// `error` once the temporary `temp`, which the copy failed to finish, is removed. A temporary that stays is named.
fn without_temporary(error: Error, temp: &Path, side: &HostSide) -> Error {
    error.after_cleanup(
        &format!("the temporary file {}", side.name(temp)),
        remove_if_present(temp),
    )
}

/// Moves the temporary `temp` over `target`, replacing a file or a symlink there, and removes `temp` when it fails.
fn replace(side: &HostSide, temp: &Path, target: &Path) -> Result<()> {
    side.host.replace_file(temp, target).map_err(|e| {
        without_temporary(
            local_error("cannot move the copy into place at", side.name(target), e),
            temp,
            side,
        )
    })
}

/// Makes the downloaded `temp` the file `target`, with the modification time and execute bit of the guest file. Both
/// are set on `temp`, before it replaces anything: a failure leaves `target` as it was.
fn finish_file(temp: &Path, target: &Path, executable: bool, mtime: Option<u32>, side: &HostSide) -> Result<()> {
    let prepare = || -> Result<()> {
        if let Some(seconds) = mtime {
            side.host
                .set_file_mtime(temp, seconds)
                .map_err(|e| local_error("cannot set the time of", side.name(temp), e))?;
        }
        if executable {
            side.host
                .set_executable(temp)
                .map_err(|e| local_error("cannot make executable", side.name(temp), e))?;
        }
        Ok(())
    };
    prepare().map_err(|e| without_temporary(e, temp, side))?;
    replace(side, temp, target)
}

// The side ============================================================================================================

/// The host end of a copy.
pub(super) struct HostSide<'a> {
    pub(super) host: &'a dyn Host,
    /// What the user typed for the host path of the copy, if they did.
    typed: Typed,
}

impl<'a> HostSide<'a> {
    pub(super) fn new(host: &'a dyn Host, typed: Typed) -> HostSide<'a> {
        HostSide { host, typed }
    }

    /// `path` in the words of a message.
    fn name(&self, path: &Path) -> String {
        self.typed.name(path.display().to_string())
    }
}

impl CopySide for HostSide<'_> {
    type Path = PathBuf;

    const NAME: &'static str = "host";
    const AS_DESTINATION: &'static str = "this host";
    const SOURCE_TIMES: &'static str = "1970 to 2106";

    fn show(&self, path: &PathBuf) -> String {
        self.name(path)
    }

    fn join(&self, base: &PathBuf, name: &str) -> PathBuf {
        base.join(name)
    }

    fn parent(&self, path: &PathBuf) -> Option<PathBuf> {
        path.parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(Path::to_owned)
    }

    fn file_name(&self, path: &PathBuf) -> Result<String> {
        without_trailing_separator(path)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .ok_or_else(|| Error::new(ErrorKind::Usage, format!("{} has no usable file name", self.name(path))))
    }

    fn chain(&self, path: &PathBuf) -> Vec<(Option<String>, PathBuf)> {
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

    fn file_name_problem(&self, name: &str) -> Option<String> {
        self.host.file_name_problem(name)
    }

    fn name_key(&self, name: &str) -> String {
        self.host.name_key(name)
    }

    fn inspect(&self, path: &PathBuf, follow: bool) -> Result<Kind> {
        let metadata = if follow {
            std::fs::metadata(path)
        } else {
            std::fs::symlink_metadata(path)
        }
        .map_err(|e| local_error("the host source", self.name(path), e))?;
        kind_of(path, &metadata, self)
    }

    fn walk(&self, dir: &PathBuf) -> Result<Vec<Item>> {
        let mut items = Vec::new();
        let mut pending = vec![(dir.to_owned(), Vec::<String>::new())];
        while let Some((path, rel)) = pending.pop() {
            let mut names = Vec::new();
            for entry in std::fs::read_dir(&path).map_err(|e| local_error("cannot list", self.name(&path), e))? {
                let entry = entry.map_err(|e| local_error("cannot list", self.name(&path), e))?;
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
                            self.name(&child)
                        ),
                    ));
                };
                let metadata =
                    std::fs::symlink_metadata(&child).map_err(|e| local_error("cannot read", self.name(&child), e))?;
                let kind = kind_of(&child, &metadata, self)?;
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

    fn present(&self, path: &PathBuf, follow: bool) -> Result<Present> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                Ok(Present::Link(if follow && leads_to_dir(path, self)? {
                    Beyond::Dir
                } else {
                    Beyond::NotDir
                }))
            }
            Ok(metadata) if metadata.is_dir() => Ok(Present::Dir),
            Ok(_) => Ok(Present::File),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Present::Nothing),
            Err(e) => Err(local_error("cannot inspect", self.name(path), e)),
        }
    }

    fn create_dir(&self, path: &PathBuf) -> Result<bool> {
        // Looked at first: creating an existing Windows drive or share root fails with access denied, not with
        // already exists.
        if path.is_dir() {
            return Ok(false);
        }
        let made = |result: std::io::Result<()>| match result {
            Ok(()) => Ok(true),
            // Whatever is there must be a directory (a link to one counts), or the copy cannot go on. Another
            // process may have made it since the look above.
            Err(_) if path.is_dir() => Ok(false),
            Err(e) => Err(local_error("cannot create the directory", self.name(path), e)),
        };
        match std::fs::create_dir(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = self.parent(path) {
                    std::fs::create_dir_all(&parent)
                        .map_err(|e| local_error("cannot create the directory", self.name(&parent), e))?;
                }
                made(std::fs::create_dir(path))
            }
            other => made(other),
        }
    }

    fn remove_link(&self, path: &PathBuf) -> Result<()> {
        // A symlink to a directory is removed as a directory on Windows and as a file elsewhere.
        std::fs::remove_file(path)
            .or_else(|as_file| std::fs::remove_dir(path).map_err(|as_dir| link_removal_failure(as_file, as_dir)))
            .map_err(|e| local_error("cannot replace the symlink", self.name(path), e))
    }

    fn probe_links(&self, dest: &PathBuf) -> Result<Option<String>> {
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
            .ok_or_else(|| Error::new(ErrorKind::Io, format!("no directory above {} exists", self.name(dest))))?;
        let probe = dir.join(partial_name());
        if let Err(e) = self.host.make_symlink(".", &probe, false) {
            return Ok(Some(e.to_string()));
        }
        std::fs::remove_file(&probe)
            .map_err(|e| local_error("cannot remove the test symlink", self.name(&probe), e))?;
        Ok(None)
    }

    fn set_dir_time(&self, path: &PathBuf, seconds: u32) -> Result<()> {
        filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(i64::from(seconds), 0))
            .map_err(|e| local_error("cannot set the time of", self.name(path), e))
    }

    fn put_link(&self, text: &str, to_dir: bool, target: &PathBuf, what: &str) -> Result<()> {
        let temp = temp_beside(target);
        self.host.make_symlink(text, &temp, to_dir).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot copy {what} to {} on this host: {e}", self.name(target)),
                e,
            )
        })?;
        replace(self, &temp, target)
    }
}

impl Receives<GuestSide<'_>> for HostSide<'_> {
    fn put_file(
        &self,
        source: &GuestSide<'_>,
        from: &String,
        executable: bool,
        mtime: Option<u32>,
        to: &PathBuf,
    ) -> Result<()> {
        let temp = temp_beside(to);
        match source.download(from, &temp, &self.name(to)) {
            Ok(()) => finish_file(&temp, to, executable, mtime, self),
            Err(e) => Err(without_temporary(e, &temp, self)),
        }
    }

    fn announce(&self, from: &str, to: &str, total: &str) -> String {
        format!("copying the guest path {from} to {to}: {total}")
    }
}
