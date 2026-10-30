//! Asking `git` which files a repository tracks.

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

/// The root of the work tree that contains `from_dir`.
pub(super) fn work_tree_root(host: &dyn Host, from_dir: &Path) -> Result<PathBuf> {
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
        let (path, kind) = parse_record(record)?;
        match kind {
            Some(kind) => listing.tracked.push(Tracked { path, kind }),
            None => listing.uninitialized_submodules.push(path),
        }
    }
    Ok(listing)
}

/// `<mode> <object> <stage>\t<path>`. A submodule that is still listed is one git could not enter, so `None`.
fn parse_record(record: &[u8]) -> Result<(String, Option<Kind>)> {
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
    let (mode, _object, stage) = (fields.next(), fields.next(), fields.next());
    let (Some(mode), Some(stage)) = (mode, stage) else {
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
    Ok((path, kind))
}
