//! Asking `git` which files a repository tracks.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::host::Host;
use crate::{Error, ErrorKind, Result};

const MODE_FILE: u32 = 0o100644;
const MODE_EXECUTABLE: u32 = 0o100755;
const MODE_SYMLINK: u32 = 0o120000;
const MODE_SUBMODULE: u32 = 0o160000;

/// What a tracked path is, by its index mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Executable,
    Symlink,
}

/// One entry of the index.
#[derive(Debug)]
pub(super) struct Tracked {
    /// Relative to the work tree root, `/`-separated.
    pub path: String,
    pub kind: Kind,
    /// The blob (hex) in the index: for a symlink, its link text.
    pub object: String,
}

#[derive(Debug)]
pub(super) struct Listing {
    pub tracked: Vec<Tracked>,
    pub uninitialized_submodules: Vec<String>,
}

/// A `git` command in `dir`. It ignores the repository the caller's environment points at (a git hook sets
/// `GIT_DIR` and `GIT_INDEX_FILE`), and starts no console window.
pub(super) fn command(host: &dyn Host, dir: &Path) -> Command {
    let mut command = Command::new("git");
    for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR"] {
        command.env_remove(name);
    }
    command.current_dir(dir);
    host.quiet(&mut command);
    command
}

fn run(mut command: Command) -> Result<Output> {
    command
        .output()
        .map_err(|e| Error::io(ErrorKind::Io, "cannot run `git` (is it installed and on PATH?)", e))
}

fn not_a_directory(from_dir: &Path) -> Error {
    Error::new(
        ErrorKind::Recipe,
        format!(
            "`{}` is not a directory, so `copy: git-tracked` has nothing to copy",
            from_dir.display()
        ),
    )
}

/// The root of the work tree that contains `from_dir`.
pub(super) fn work_tree_root(host: &dyn Host, from_dir: &Path) -> Result<PathBuf> {
    // Git cannot start in a directory that is not there, and `run` would blame git for it.
    match std::fs::metadata(from_dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(not_a_directory(from_dir)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(not_a_directory(from_dir)),
        Err(e) => {
            return Err(Error::io(
                ErrorKind::Io,
                format!("cannot read {}", from_dir.display()),
                e,
            ));
        }
    }
    let mut cmd = command(host, from_dir);
    cmd.args(["rev-parse", "--is-inside-work-tree", "--show-cdup"]);
    let out = run(cmd)?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    // Inside a `.git` directory git succeeds and prints `false`.
    if !out.status.success() || lines.next() != Some("true") {
        return Err(Error::new(
            ErrorKind::Recipe,
            format!(
                "`{}` is not inside a git work tree, so `copy: git-tracked` has nothing to copy (git: {})",
                from_dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    // `--show-cdup` is `../` repeated, or empty at the root: ASCII, so no encoding is involved.
    Ok(match lines.next() {
        None | Some("") => from_dir.to_owned(),
        Some(up) => from_dir.join(up),
    })
}

/// The index of the work tree at `root`, with the index of every initialized submodule in place of the submodule.
pub(super) fn list(host: &dyn Host, root: &Path) -> Result<Listing> {
    let mut cmd = command(host, root);
    cmd.args(["ls-files", "-z", "-s", "--recurse-submodules"]);
    let out = run(cmd)?;
    if !out.status.success() {
        return Err(Error::new(
            ErrorKind::Recipe,
            format!(
                "cannot list the files git tracks in `{}`: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    let mut listing = Listing {
        tracked: Vec::new(),
        uninitialized_submodules: Vec::new(),
    };
    for record in out.stdout.split(|&b| b == 0).filter(|record| !record.is_empty()) {
        let (path, kind, object) = parse_record(record)?;
        match kind {
            Some(kind) => listing.tracked.push(Tracked { path, kind, object }),
            None => {
                refuse_unlisted_work_tree(root, &path)?;
                listing.uninitialized_submodules.push(path);
            }
        }
    }
    Ok(listing)
}

/// Fails when the gitlink `path` holds a work tree although git listed no files of it: an embedded repository, or a
/// submodule git considers inactive. Skipping it as "uninitialized" would leave files that are on disk out.
fn refuse_unlisted_work_tree(root: &Path, path: &str) -> Result<()> {
    let marker = root.join(path).join(".git");
    let present = marker
        .try_exists()
        .map_err(|e| Error::io(ErrorKind::Io, format!("cannot read {}", marker.display()), e))?;
    if !present {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::Recipe,
        format!(
            "cannot copy `{}`: it is a git repository, but git does not list its files (make it a submodule, or remove it from the index)",
            path.escape_debug()
        ),
    ))
}

/// `<mode> <object> <stage>\t<path>`. A submodule that is still listed is one git could not enter, so `None`.
fn parse_record(record: &[u8]) -> Result<(String, Option<Kind>, String)> {
    let malformed = || {
        Error::new(
            ErrorKind::Internal,
            format!(
                "unexpected `git ls-files` record: {}",
                String::from_utf8_lossy(record).escape_debug()
            ),
        )
    };
    let tab = record.iter().position(|&b| b == b'\t').ok_or_else(malformed)?;
    let (meta, path) = (&record[..tab], &record[tab + 1..]);
    let path = String::from_utf8(path.to_vec()).map_err(|_| {
        Error::new(
            ErrorKind::Recipe,
            format!(
                "cannot copy `{}`: the name is not UTF-8",
                String::from_utf8_lossy(path).escape_debug()
            ),
        )
    })?;
    let meta = std::str::from_utf8(meta).map_err(|_| malformed())?;
    let mut fields = meta.split(' ');
    let (mode, object, stage) = (fields.next(), fields.next(), fields.next());
    let (Some(mode), Some(object), Some(stage)) = (mode, object, stage) else {
        return Err(malformed());
    };
    if stage != "0" {
        return Err(Error::new(
            ErrorKind::Recipe,
            format!(
                "cannot copy `{}`: it has unresolved merge conflicts",
                path.escape_debug()
            ),
        ));
    }
    let kind = match u32::from_str_radix(mode, 8).map_err(|_| malformed())? {
        MODE_FILE => Some(Kind::File),
        MODE_EXECUTABLE => Some(Kind::Executable),
        MODE_SYMLINK => Some(Kind::Symlink),
        MODE_SUBMODULE => None,
        other => {
            return Err(Error::new(
                ErrorKind::Recipe,
                format!(
                    "cannot copy `{}`: git mode {other:o} is not a file",
                    path.escape_debug()
                ),
            ));
        }
    };
    Ok((path, kind, object.to_owned()))
}

/// The link text of the tracked symlink `item`, as git stores it: the same on every host, where a link read back
/// from disk can have its separators changed. Git runs in the directory that holds the link, so a symlink in a
/// submodule is looked up in the submodule's repository.
pub(super) fn link_text(host: &dyn Host, root: &Path, item: &Tracked) -> Result<String> {
    let on_disk = root.join(&item.path);
    let dir = on_disk.parent().expect("a tracked path is below the root");
    let mut cmd = command(host, dir);
    cmd.args(["cat-file", "blob", &item.object]);
    let out = run(cmd)?;
    if !out.status.success() {
        return Err(Error::new(
            ErrorKind::Recipe,
            format!(
                "cannot read the link text of `{}` from git: {}",
                item.path.escape_debug(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    String::from_utf8(out.stdout).map_err(|_| {
        Error::new(
            ErrorKind::Recipe,
            format!("cannot copy `{}`: its link text is not UTF-8", item.path.escape_debug()),
        )
    })
}
