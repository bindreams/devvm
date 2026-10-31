//! `copy: { from: git-tracked }`: a snapshot of the files git tracks, as a tar plus the manifest that keys its layer.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::exit::Progress;
use crate::host::Host;
use crate::recipe::hash::CopyManifest;
use crate::{Error, ErrorKind, Result};

mod archive;
mod git;

/// A full disk leaves nothing to resume: the copy starts over.
const FULL_DISK_HINT: &str = "free some space and run again";

#[cfg(test)]
#[path = "copy_tests.rs"]
pub(crate) mod copy_tests;

#[cfg(all(test, unix))]
#[path = "copy_unix_tests.rs"]
mod copy_unix_tests;

#[cfg(test)]
#[path = "copy/heap_tests.rs"]
pub(crate) mod heap_tests;

/// The files of one `copy` step.
#[derive(Debug)]
pub(crate) struct Snapshot {
    /// What the layer key hashes.
    pub manifest: CopyManifest,
    /// A tar of exactly the manifest's files, in manifest order. The caller removes it.
    pub archive: PathBuf,
}

/// Snapshots the files git tracks in the repository that contains `from_dir`, as they are on disk.
///
/// - Entries come from the index. A tracked file missing on disk is skipped, so a pending delete leaves the
///   copy without it; so is one that is a directory now, with a `progress` line. An initialized submodule is
///   included recursively; an uninitialized one (no work tree in its directory) is skipped with a `progress`
///   line. A symlink stays a symlink, with git's link text. File contents are not converted (no
///   `core.autocrlf`).
/// - Each file is read once, and the same bytes go into the tar and into the manifest hash.
/// - `read_only` clears the write bits of the files in the tar. The tar has no directory entries, so extraction
///   leaves directories writable.
/// - The tar is a new file in `into`, which must exist. It has fixed timestamps and no owner, so equal manifests
///   give equal bytes.
///
/// The tracked tree must not contain case-insensitive name collisions, names with a trailing dot, or Windows device
/// names (`nul`, `con`); devvm does not check, the contract of `devvm cp`. Names are copied as git lists them.
///
/// Fails with `recipe` when `from_dir` is not in a work tree, a tracked path is not UTF-8, the index holds an
/// unresolved merge conflict, a tracked file is a symlink now (or something else that is not a file), or a
/// directory holds a repository that git neither lists nor treats as a submodule.
pub(crate) fn snapshot(
    host: &dyn Host,
    from_dir: &Path,
    read_only: bool,
    into: &Path,
    progress: &dyn Progress,
) -> Result<Snapshot> {
    let root = git::work_tree_root(host, from_dir)?;
    let mut listing = git::list(host, &root)?;
    for submodule in &listing.uninitialized_submodules {
        progress.message(&format!(
            "skipping uninitialized submodule `{}`",
            submodule.escape_debug()
        ));
    }
    listing.tracked.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));

    let file = tempfile::Builder::new()
        .prefix("devvm-copy-")
        .suffix(".tar")
        .tempfile_in(into)
        .map_err(|e| Error::write(into, e, FULL_DISK_HINT))?;
    let mut out = BufWriter::new(file.as_file());
    let entries = archive::write(
        host,
        progress,
        &root,
        &listing.tracked,
        read_only,
        file.path(),
        &mut out,
    )?;
    out.flush().map_err(|e| Error::write(file.path(), e, FULL_DISK_HINT))?;
    drop(out);
    let manifest = CopyManifest::new(entries)?;
    let (_, archive) = file.keep().map_err(|e| {
        Error::io(
            ErrorKind::Io,
            format!("cannot keep the archive in {}", into.display()),
            e.error,
        )
    })?;
    Ok(Snapshot { manifest, archive })
}
