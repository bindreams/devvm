//! Writing the tracked files to a tar, hashing them on the way.

use std::io::{self, Read, Write};
use std::path::Path;

use sha2::{Digest, Sha256 as Hasher};
use tar::{Builder, EntryType, Header};

use super::git::{Kind, Tracked};
use crate::recipe::hash::{EntryContent, ManifestEntry};
use crate::{Error, ErrorKind, Result, Sha256};

#[cfg(test)]
#[path = "archive_tests.rs"]
mod archive_tests;

/// The same time on every entry, so equal manifests give equal archives. Not zero: GNU tar warns about it.
const MTIME: u64 = 946_684_800;

/// Writes the files `tracked` (sorted by path) found under `root` to `out`, and returns their manifest entries in
/// the same order. A tracked path that is gone from disk is left out.
pub(super) fn write(root: &Path, tracked: &[Tracked], read_only: bool, out: impl Write) -> Result<Vec<ManifestEntry>> {
    let mut builder = Builder::new(out);
    let mut entries = Vec::with_capacity(tracked.len());
    for item in tracked {
        let on_disk = root.join(&item.path);
        let meta = match std::fs::symlink_metadata(&on_disk) {
            Ok(meta) => meta,
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => continue,
            Err(e) => {
                return Err(Error::io(
                    ErrorKind::Io,
                    format!("cannot read {}", on_disk.display()),
                    e,
                ));
            }
        };
        let entry = match item.kind {
            Kind::Symlink => {
                let target = link_text(&on_disk, &meta, &item.path)?;
                append_symlink(&mut builder, &item.path, &target)?;
                ManifestEntry {
                    path: item.path.clone(),
                    mode: 0o120000,
                    content: EntryContent::SymlinkTarget(target),
                }
            }
            Kind::File | Kind::Executable => {
                if !meta.is_file() {
                    return Err(Error::new(
                        ErrorKind::Recipe,
                        format!(
                            "cannot copy `{}`: git tracks a file but {} is not one",
                            item.path.escape_debug(),
                            on_disk.display()
                        ),
                    ));
                }
                let executable = item.kind == Kind::Executable;
                let file = std::fs::File::open(&on_disk)
                    .map_err(|e| Error::io(ErrorKind::Io, format!("cannot open {}", on_disk.display()), e))?;
                let size = file
                    .metadata()
                    .map_err(|e| Error::io(ErrorKind::Io, format!("cannot read {}", on_disk.display()), e))?
                    .len();
                let sha = append_file(&mut builder, &item.path, tar_mode(executable, read_only), size, file)?;
                ManifestEntry {
                    path: item.path.clone(),
                    mode: if executable { 0o100755 } else { 0o100644 },
                    content: EntryContent::FileSha256(sha),
                }
            }
        };
        entries.push(entry);
    }
    builder
        .finish()
        .map_err(|e| Error::io(ErrorKind::Io, "cannot finish the archive", e))?;
    Ok(entries)
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

/// The text of a tracked symlink: the link itself, or, where the checkout cannot make links, the plain file that
/// stands for it.
fn link_text(on_disk: &Path, meta: &std::fs::Metadata, path: &str) -> Result<String> {
    let not_utf8 = || {
        Error::new(
            ErrorKind::Recipe,
            format!("cannot copy `{}`: its link text is not UTF-8", path.escape_debug()),
        )
    };
    let read_failed = |e| Error::io(ErrorKind::Io, format!("cannot read {}", on_disk.display()), e);
    if meta.file_type().is_symlink() {
        std::fs::read_link(on_disk)
            .map_err(read_failed)?
            .into_os_string()
            .into_string()
            .map_err(|_| not_utf8())
    } else if meta.is_file() {
        String::from_utf8(std::fs::read(on_disk).map_err(read_failed)?).map_err(|_| not_utf8())
    } else {
        Err(Error::new(
            ErrorKind::Recipe,
            format!(
                "cannot copy `{}`: git tracks a symlink but {} is neither a link nor a file",
                path.escape_debug(),
                on_disk.display()
            ),
        ))
    }
}

fn append_symlink<W: Write>(builder: &mut Builder<W>, path: &str, target: &str) -> Result<()> {
    let mut header = header(EntryType::Symlink, 0o777, 0);
    builder
        .append_link(&mut header, path, target)
        .map_err(|e| Error::io(ErrorKind::Io, format!("cannot archive `{}`", path.escape_debug()), e))
}

/// Counts and hashes what passes through.
struct Hashing<R> {
    inner: R,
    hasher: Hasher,
    count: u64,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
}

/// Appends one regular file whose header says `size` bytes, streaming `reader` into the tar while hashing it, and
/// returns the hash of exactly the bytes archived. A reader that ends before `size` bytes or has more after them
/// is an `io` error naming `path`: the file changed under the copy.
pub(super) fn append_file<W: Write, R: Read>(
    builder: &mut Builder<W>,
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
    };
    builder
        .append_data(&mut header, path, &mut hashing)
        .map_err(|e| Error::io(ErrorKind::Io, format!("cannot archive `{}`", path.escape_debug()), e))?;
    if hashing.count != size {
        return Err(changed(hashing.count));
    }
    let mut extra = [0u8; 1];
    let more = hashing
        .inner
        .into_inner()
        .read(&mut extra)
        .map_err(|e| Error::io(ErrorKind::Io, format!("cannot read `{}`", path.escape_debug()), e))?;
    if more != 0 {
        return Err(changed(size + 1));
    }
    Ok(Sha256::from_bytes(hashing.hasher.finalize().into()))
}
