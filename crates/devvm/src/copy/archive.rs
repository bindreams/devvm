//! Writing the tracked files to a tar, hashing them on the way.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::path::{Component, Path};

use sha2::{Digest, Sha256 as Hasher};
use tar::{Builder, EntryType, Header};

use super::FULL_DISK_HINT;
use super::git::{self, Git, Kind, Tracked};
use crate::exit::Progress;
use crate::host::{Entry, is_absent};
use crate::recipe::hash::{EntryContent, ManifestEntry};
use crate::{Error, ErrorKind, Result, Sha256};

#[cfg(test)]
#[path = "archive_tests.rs"]
mod archive_tests;

/// The same time on every entry, so equal manifests give equal archives. Not zero: GNU tar warns about it.
const MTIME: u64 = 946_684_800;

/// Writes the files `tracked` (sorted by path) found under `root` to `out`, which is the file `dest`, and returns
/// their manifest entries in input order. What is skipped and what fails is listed at [`super::snapshot`].
pub(super) fn write(
    git: &Git,
    progress: &dyn Progress,
    root: &Path,
    tracked: &[Tracked],
    read_only: bool,
    dest: &Path,
    out: impl Write,
) -> Result<Vec<ManifestEntry>> {
    let mut builder = Builder::new(out);
    let mut entries = Vec::with_capacity(tracked.len());
    let mut real_directories = HashSet::new();
    for item in tracked {
        if git.host().cannot_hold_path(&item.path) {
            continue;
        }
        check_one_component_per_segment(&item.path)?;
        check_parents(root, &item.path, &mut real_directories)?;
        let on_disk = root.join(&item.path);
        let found = git
            .host()
            .open_entry(root, &item.path)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot read {}", on_disk.display()), e))?;
        let entry = match (found, item.kind) {
            (Entry::Absent, _) => continue,
            (Entry::Directory, kind) => {
                progress.message(&format!(
                    "skipping `{}`: git tracks a {} there, but it is a directory now",
                    item.path.escape_debug(),
                    if kind == Kind::Symlink { "symlink" } else { "file" }
                ));
                continue;
            }
            (found, Kind::Symlink) => {
                check_stands_for_link(git, &on_disk, &found, item)?;
                let target = git::link_text(git, item)?;
                append_symlink(&mut builder, dest, &item.path, &target)?;
                ManifestEntry {
                    path: item.path.clone(),
                    mode: item.kind.mode(),
                    content: EntryContent::SymlinkTarget(target),
                }
            }
            (Entry::File(file), Kind::File | Kind::Executable) => {
                let size = file
                    .metadata()
                    .map_err(|e| Error::io(ErrorKind::Io, format!("cannot read {}", on_disk.display()), e))?
                    .len();
                let mode = tar_mode(item.kind == Kind::Executable, read_only);
                let sha = append_file(&mut builder, dest, &item.path, mode, size, file)?;
                ManifestEntry {
                    path: item.path.clone(),
                    mode: item.kind.mode(),
                    content: EntryContent::FileSha256(sha),
                }
            }
            (found, Kind::File | Kind::Executable) => {
                let found = match found {
                    Entry::Other(what) => format!("neither a file nor a directory ({what})"),
                    _ => "a symbolic link".to_owned(),
                };
                return Err(Error::new(
                    ErrorKind::Recipe,
                    format!(
                        "cannot copy `{}`: git tracks a file there but {} is {found} (commit the change or restore the file)",
                        item.path.escape_debug(),
                        on_disk.display()
                    ),
                ));
            }
        };
        entries.push(entry);
    }
    builder.finish().map_err(|e| Error::write(dest, e, FULL_DISK_HINT))?;
    Ok(entries)
}

/// Fails when a directory above `path` (relative to `root`, `/`-separated) is a symlink, as git does. A missing
/// directory ends the check: `path` is then gone from disk too. `real_directories` holds the directories already
/// seen, and spares a `stat` per file.
fn check_parents(root: &Path, path: &str, real_directories: &mut HashSet<String>) -> Result<()> {
    for (slash, _) in path.match_indices('/') {
        let directory = &path[..slash];
        if real_directories.contains(directory) {
            continue;
        }
        let on_disk = root.join(directory);
        match std::fs::symlink_metadata(&on_disk) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(Error::new(
                    ErrorKind::Recipe,
                    format!(
                        "cannot copy `{}`: `{}` is a symbolic link, and a copy does not follow one in the middle of a path",
                        path.escape_debug(),
                        directory.escape_debug()
                    ),
                ));
            }
            Ok(_) => {
                real_directories.insert(directory.to_owned());
            }
            Err(e) if is_absent(&e) => return Ok(()),
            Err(e) => {
                return Err(Error::io(
                    ErrorKind::Io,
                    format!("cannot read {}", on_disk.display()),
                    e,
                ));
            }
        }
    }
    Ok(())
}

/// Fails when this host reads `path` (`/`-separated, as git lists it) as other than one component per segment. A
/// backslash is a legal byte in a POSIX file name, but on Windows it separates components: reading or archiving
/// the name there would reach a different file.
pub(super) fn check_one_component_per_segment(path: &str) -> Result<()> {
    let components = Path::new(path).components();
    if components.clone().all(|c| matches!(c, Component::Normal(_))) && components.count() == path.split('/').count() {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::Recipe,
        format!(
            "cannot copy `{}`: this host cannot hold a file with that name",
            path.escape_debug()
        ),
    ))
}

fn tar_mode(executable: bool, read_only: bool) -> u32 {
    match (executable, read_only) {
        (false, false) => 0o644,
        (true, false) => 0o755,
        (false, true) => 0o444,
        (true, true) => 0o555,
    }
}

fn header(kind: EntryType, mode: u32, size: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(mode);
    header.set_size(size);
    header.set_mtime(MTIME);
    header.set_uid(0);
    header.set_gid(0);
    header
}

/// A tracked symlink is a link on disk or, where the checkout writes links as files (`core.symlinks` false), the
/// plain file that stands for it. Elsewhere a regular file is a typechange. Its target comes from git, never from
/// disk.
fn check_stands_for_link(git: &Git, on_disk: &Path, found: &Entry, item: &Tracked) -> Result<()> {
    let found = match found {
        Entry::Symlink => return Ok(()),
        Entry::File(_) if git::links_are_files(git, item)? => return Ok(()),
        Entry::File(_) => "a regular file".to_owned(),
        Entry::Other(what) => format!("neither a link nor a file ({what})"),
        _ => "neither a link nor a file".to_owned(),
    };
    Err(Error::new(
        ErrorKind::Recipe,
        format!(
            "cannot copy `{}`: git tracks a symlink there but {} is {found} (commit the change or restore the link)",
            item.path.escape_debug(),
            on_disk.display()
        ),
    ))
}

/// The size of a tar header's link name field. A text this long or longer goes in a GNU `K` entry.
const LINK_NAME_FIELD: usize = 100;

/// Appends the symlink `path` with the link text `target`, byte for byte: the tar's own link name handling would
/// rewrite `a//b`, `a/./b` and (on Windows) a backslash, and the archive would then differ from the manifest.
pub(super) fn append_symlink<W: Write>(builder: &mut Builder<W>, dest: &Path, path: &str, target: &str) -> Result<()> {
    let write_error = |e| Error::write(dest, e, FULL_DISK_HINT);
    let mut link = header(EntryType::Symlink, 0o777, 0);
    if target.contains('\0') {
        return Err(Error::new(
            ErrorKind::Recipe,
            format!("cannot copy `{}`: its link text contains a NUL", path.escape_debug()),
        ));
    }
    if target.len() < LINK_NAME_FIELD {
        link.set_link_name_literal(target).map_err(write_error)?;
    } else {
        let cut = (0..LINK_NAME_FIELD)
            .rev()
            .find(|&i| target.is_char_boundary(i))
            .unwrap_or(0);
        link.set_link_name_literal(&target[..cut]).map_err(write_error)?;
        let mut long = header(EntryType::GNULongLink, 0o644, target.len() as u64 + 1);
        let name = b"././@LongLink";
        long.as_gnu_mut().expect("a GNU header").name[..name.len()].copy_from_slice(name);
        long.set_cksum();
        let text = target.as_bytes().chain(io::repeat(0).take(1));
        builder.append(&long, text).map_err(write_error)?;
    }
    builder.append_data(&mut link, path, io::empty()).map_err(write_error)
}

/// Counts and hashes what passes through, and notes whether the source failed: the tar reports that and its own
/// write failures alike.
struct Hashing<R> {
    inner: R,
    hasher: Hasher,
    count: u64,
    read_failed: bool,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // `io::copy` retries an interrupted read, so that one is not a failure of the source.
        let n = self
            .inner
            .read(buf)
            .inspect_err(|e| self.read_failed |= e.kind() != io::ErrorKind::Interrupted)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
}

/// Appends one regular file whose header says `size` bytes, streaming `reader` into the tar while hashing it, and
/// returns the hash of exactly the bytes archived. A reader that ends before `size` bytes or has more after them
/// is an `io` error naming `path`: the file changed under the copy. A failure writing the tar, the file `dest`, is
/// `disk-full` or `io` naming `dest`.
pub(super) fn append_file<W: Write, R: Read>(
    builder: &mut Builder<W>,
    dest: &Path,
    path: &str,
    mode: u32,
    size: u64,
    reader: R,
) -> Result<Sha256> {
    let changed = |read: u64| {
        Error::new(
            ErrorKind::Io,
            format!(
                "`{}` changed while it was being copied (expected {size} bytes, found {})",
                path.escape_debug(),
                if read < size {
                    read.to_string()
                } else {
                    format!("more than {size}")
                }
            ),
        )
    };
    let mut header = header(EntryType::Regular, mode, size);
    let mut hashing = Hashing {
        inner: reader.take(size),
        hasher: Hasher::new(),
        count: 0,
        read_failed: false,
    };
    builder.append_data(&mut header, path, &mut hashing).map_err(|e| {
        if hashing.read_failed {
            Error::io(ErrorKind::Io, format!("cannot read `{}`", path.escape_debug()), e)
        } else {
            Error::write(dest, e, FULL_DISK_HINT)
        }
    })?;
    if hashing.count != size {
        return Err(changed(hashing.count));
    }
    let mut extra = [0u8; 1];
    let mut rest = hashing.inner.into_inner();
    let more = loop {
        match rest.read(&mut extra) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            other => break other,
        }
    }
    .map_err(|e| Error::io(ErrorKind::Io, format!("cannot read `{}`", path.escape_debug()), e))?;
    if more != 0 {
        return Err(changed(size + 1));
    }
    Ok(Sha256::from_bytes(hashing.hasher.finalize().into()))
}
