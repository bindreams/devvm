//! Asking `git` which files a repository tracks.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use os_str_bytes::OsStringBytes;

use crate::exit::Progress;
use crate::host::{Host, is_absent};
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "git_tests.rs"]
mod git_tests;

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

impl Kind {
    /// The index mode git records for this kind.
    pub(super) fn mode(self) -> u32 {
        match self {
            Kind::File => MODE_FILE,
            Kind::Executable => MODE_EXECUTABLE,
            Kind::Symlink => MODE_SYMLINK,
        }
    }
}

/// One entry of the index.
#[derive(Debug)]
pub(super) struct Tracked {
    /// Relative to the work tree root, `/`-separated.
    pub path: String,
    pub kind: Kind,
    /// The blob (hex) in the index: for a symlink, its link text.
    pub object: String,
    /// The work tree of the repository whose index lists the entry: the root, or a submodule's. Its objects and its
    /// configuration are the entry's.
    pub repo: Arc<Path>,
}

#[derive(Debug)]
pub(super) struct Listing {
    pub tracked: Vec<Tracked>,
    pub uninitialized_submodules: Vec<String>,
}

/// The start of the note that lists the variables git will not see.
pub(super) const UNSET_NOTE_PREFIX: &str = "The following variables have been unset: ";

/// The `git` a snapshot runs: the same commands in every call, in an environment that holds none of this process's
/// `GIT_*` variables but the ones that add to git's configuration. A git hook sets `GIT_DIR` and `GIT_INDEX_FILE`.
/// Any other `GIT_*` variable can make git read another repository or write into the output the snapshot parses.
pub(super) struct Git<'a> {
    host: &'a dyn Host,
    /// The variables removed, sorted, as the environment names them.
    cleared: Vec<OsString>,
}

impl<'a> Git<'a> {
    /// Reads this process's environment, and says through `progress` which variables git will not see.
    pub(super) fn from_process(host: &'a dyn Host, progress: &dyn Progress) -> Git<'a> {
        Git::from_names(host, std::env::vars_os().map(|(name, _)| name), progress)
    }

    fn from_names(host: &'a dyn Host, names: impl IntoIterator<Item = OsString>, progress: &dyn Progress) -> Git<'a> {
        let cleared = cleared_names(host, names);
        if !cleared.is_empty() {
            let list: Vec<_> = cleared.iter().map(|name| name.to_string_lossy()).collect();
            progress.message(&format!("{UNSET_NOTE_PREFIX}{}", list.join(", ")));
        }
        Git { host, cleared }
    }

    /// The host the snapshot runs on.
    pub(super) fn host(&self) -> &'a dyn Host {
        self.host
    }

    /// A `git` command in `dir` without the cleared variables, and with no console window.
    fn command(&self, dir: &Path) -> Command {
        let mut command = Command::new("git");
        for name in &self.cleared {
            command.env_remove(name);
        }
        command.current_dir(dir);
        self.host.quiet(&mut command);
        command
    }
}

/// The names in `names` that `git` must not inherit, sorted and without repeats: every `GIT_*` variable except
/// `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT` and the `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_VALUE_<n>` pairs, which add
/// to the configuration, so the user's settings still reach git. `host` decides what the same name is (Windows ignores
/// case, by the OS's own table).
pub(super) fn cleared_names(host: &dyn Host, names: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut cleared: Vec<OsString> = names
        .into_iter()
        .filter(|name| host.env_name_strip_prefix(name, "GIT_").is_some() && !adds_to_configuration(host, name))
        .collect();
    cleared.sort();
    cleared.dedup();
    cleared
}

fn adds_to_configuration(host: &dyn Host, name: &OsStr) -> bool {
    let strip = |prefix| host.env_name_strip_prefix(name, prefix);
    let numbered = |prefix| strip(prefix).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    strip("GIT_CONFIG_PARAMETERS").is_some_and(|rest| rest.is_empty())
        || strip("GIT_CONFIG_COUNT").is_some_and(|rest| rest.is_empty())
        || numbered("GIT_CONFIG_KEY_")
        || numbered("GIT_CONFIG_VALUE_")
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

/// The root of the work tree that contains `from_dir`, as git spells it: absolute, with links resolved. A path built
/// from `from_dir` and the number of `../` git prints is not that root when `from_dir` goes through a link, since git
/// resolves the directory it starts in and Windows collapses `..` without resolving.
pub(super) fn work_tree_root(git: &Git, from_dir: &Path) -> Result<PathBuf> {
    // Git cannot start in a directory that is not there, and `run` would blame git for it.
    match std::fs::metadata(from_dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(not_a_directory(from_dir)),
        Err(e) if is_absent(&e) => return Err(not_a_directory(from_dir)),
        Err(e) => {
            return Err(Error::io(
                ErrorKind::Io,
                format!("cannot read {}", from_dir.display()),
                e,
            ));
        }
    }
    let mut cmd = git.command(from_dir);
    cmd.args(["rev-parse", "--is-inside-work-tree", "--show-toplevel"]);
    let out = run(cmd)?;
    // Inside a `.git` directory git prints `false` and then fails.
    let Some(top) = out.stdout.strip_prefix(b"true\n").filter(|_| out.status.success()) else {
        return Err(Error::new(
            ErrorKind::Recipe,
            format!(
                "`{}` is not inside a git work tree, so `copy: git-tracked` has nothing to copy (git: {})",
                from_dir.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    };
    let top = top.strip_suffix(b"\n").unwrap_or(top);
    OsString::from_io_vec(top.to_vec()).map(PathBuf::from).ok_or_else(|| {
        Error::new(
            ErrorKind::Internal,
            format!(
                "unexpected `git rev-parse --show-toplevel` output: {}",
                String::from_utf8_lossy(top).escape_debug()
            ),
        )
    })
}

/// The index of the work tree at `root`, with the index of every submodule that is checked out in place of the
/// submodule. Each repository is listed by itself, so that a submodule git cannot enter is found and not left out.
pub(super) fn list(git: &Git, root: &Path) -> Result<Listing> {
    let mut listing = Listing {
        tracked: Vec::new(),
        uninitialized_submodules: Vec::new(),
    };
    let root: Arc<Path> = Arc::from(root);
    let out = ls_files(git, &root)?;
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
    add_records(git, &root, "", &out.stdout, &mut listing)?;
    Ok(listing)
}

fn ls_files(git: &Git, repo: &Path) -> Result<Output> {
    let mut cmd = git.command(repo);
    cmd.args(["ls-files", "-z", "-s"]);
    run(cmd)
}

/// Adds the entries `stdout` (the index of the repository `repo`, which sits at `prefix` below the snapshot's root:
/// empty, or ending in `/`) holds to `listing`, and the entries of the submodules below it.
fn add_records(git: &Git, repo: &Arc<Path>, prefix: &str, stdout: &[u8], listing: &mut Listing) -> Result<()> {
    for record in stdout.split(|&b| b == 0).filter(|record| !record.is_empty()) {
        let (path, kind, object) = parse_record(prefix, record)?;
        match kind {
            Some(kind) => listing.tracked.push(Tracked {
                path,
                kind,
                object,
                repo: Arc::clone(repo),
            }),
            None => enter_gitlink(git, repo, prefix, &path, listing)?,
        }
    }
    Ok(())
}

/// Lists the submodule that `path` (below the root, with `prefix` the part that is `repo`'s) is, when it is checked
/// out and git considers it one. A gitlink with no work tree is noted as uninitialized. Fails when its directory holds
/// a work tree that is not a submodule git can list: an embedded repository, an inactive submodule, one git cannot
/// enter. Skipping it would leave files that are on disk out.
fn enter_gitlink(git: &Git, repo: &Arc<Path>, prefix: &str, path: &str, listing: &mut Listing) -> Result<()> {
    let relative = &path[prefix.len()..];
    let dir: Arc<Path> = repo.join(relative).into();
    let marker = dir.join(".git");
    // A gitlink path that is a file now, or a name this host cannot hold, has no work tree below it, like one that
    // is gone.
    let present = match marker.try_exists() {
        Ok(present) => present,
        Err(e) if is_absent(&e) => false,
        Err(e) => return Err(Error::io(ErrorKind::Io, format!("cannot read {}", marker.display()), e)),
    };
    if !present {
        listing.uninitialized_submodules.push(path.to_owned());
        return Ok(());
    }
    // Git lists a gitlink that it does not enter as itself. It lists nothing for one it should enter and cannot.
    let mut probe = git.command(repo);
    probe.args([
        "--literal-pathspecs",
        "ls-files",
        "-z",
        "-s",
        "--recurse-submodules",
        "--",
        relative,
    ]);
    let probe = run(probe)?;
    let inside = ls_files(git, &dir)?;
    if !probe.status.success() || !inside.status.success() || lists_itself(&probe.stdout, relative) {
        return Err(Error::new(
            ErrorKind::Recipe,
            format!(
                "cannot copy `{}`: it is a git repository, but git does not list its files (it is not a submodule, or git could not enter it; make it a submodule, or remove it from the index)",
                path.escape_debug()
            ),
        ));
    }
    add_records(git, &dir, &format!("{path}/"), &inside.stdout, listing)
}

/// Whether `stdout`, from `ls-files -z -s`, holds the gitlink `relative` itself.
fn lists_itself(stdout: &[u8], relative: &str) -> bool {
    stdout.split(|&b| b == 0).any(|record| {
        let Some(tab) = record.iter().position(|&b| b == b'\t') else {
            return false;
        };
        record[..tab].starts_with(b"160000 ") && &record[tab + 1..] == relative.as_bytes()
    })
}

/// `<mode> <object> <stage>\t<path>`, with `prefix` put before the path. A submodule is `None`.
fn parse_record(prefix: &str, record: &[u8]) -> Result<(String, Option<Kind>, String)> {
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
    let path = String::from_utf8(path.to_vec())
        .map(|path| format!("{prefix}{path}"))
        .map_err(|_| {
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
/// from disk can have its separators changed.
pub(super) fn link_text(git: &Git, item: &Tracked) -> Result<String> {
    let mut cmd = git.command(&item.repo);
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

/// Whether the checkout of the repository that holds `item` writes a tracked symlink as a plain file holding its
/// link text: `core.symlinks` is false, as git reports it there (true when unset).
pub(super) fn links_are_files(git: &Git, item: &Tracked) -> Result<bool> {
    let mut cmd = git.command(&item.repo);
    cmd.args(["config", "--type=bool", "--default", "true", "--get", "core.symlinks"]);
    let out = run(cmd)?;
    if !out.status.success() {
        return Err(Error::new(
            ErrorKind::Recipe,
            format!(
                "cannot read `core.symlinks` for `{}` from git: {}",
                item.path.escape_debug(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim() == "false")
}
