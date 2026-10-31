//! The decisions of a copy that do not depend on which side is the host: where the source lands, and which names the
//! target cannot store. Both directions share them, so the rules are written once.

use std::collections::{HashMap, HashSet};

use crate::guest::GuestOs;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "plan_tests.rs"]
mod plan_tests;

/// What one entry of a copied tree is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Dir {
        /// Seconds since 1970, when the source has one SFTP can carry.
        mtime: Option<u32>,
    },
    File {
        executable: bool,
        /// Seconds since 1970, when the source has one SFTP can carry.
        mtime: Option<u32>,
        size: u64,
    },
    Link {
        /// What the link holds, `/`-separated.
        target: String,
        /// Whether it leads to a directory: a Windows host makes the two kinds differently. Only a guest source knows.
        to_dir: bool,
    },
}

impl Kind {
    pub fn is_dir(&self) -> bool {
        matches!(self, Kind::Dir { .. })
    }

    pub fn noun(&self) -> &'static str {
        match self {
            Kind::Dir { .. } => "directory",
            Kind::File { .. } => "file",
            Kind::Link { .. } => "symlink",
        }
    }
}

/// One entry of a source tree, relative to its root.
#[derive(Debug)]
pub(super) struct Item {
    pub rel: Vec<String>,
    pub kind: Kind,
}

// What lands where ====================================================================================================

/// The source of a copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Source {
    /// A directory, or a link to one that was given with a trailing `/`.
    Dir,
    /// A file, or a link copied as a link.
    Entry,
}

/// What the destination path leads to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Present {
    Nothing,
    Dir,
    File,
    /// A symlink, and what it leads to.
    Link(Beyond),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Beyond {
    Dir,
    /// A file, a dangling link, a loop: anything that is not a directory.
    NotDir,
}

/// Where the source lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Place {
    /// The destination becomes the directory and the contents merge into it. A symlink to a directory at the
    /// destination is followed, not replaced.
    Root,
    /// The entry is the destination path.
    At,
    /// The entry goes inside the destination directory, as `<destination>/<name>`.
    Inside,
}

/// The source cannot go where the destination is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Conflict {
    /// A directory onto something that is not one.
    DirOntoNonDir,
    /// A destination written with a trailing `/` that is not a directory.
    SlashOnNonDir,
}

/// Where a copy of `source` lands when the destination is `dest`, written with a trailing `/` when `slash`. The result
/// never depends on anything but these three, so a rerun lands where the first run did. A destination that is a
/// symlink counts as what it leads to: a directory is followed, anything else is a file.
pub(super) fn place(source: Source, slash: bool, dest: Present) -> std::result::Result<Place, Conflict> {
    match (source, dest) {
        (Source::Dir, Present::Nothing | Present::Dir | Present::Link(Beyond::Dir)) => Ok(Place::Root),
        (Source::Dir, Present::File | Present::Link(Beyond::NotDir)) => Err(Conflict::DirOntoNonDir),
        (Source::Entry, Present::Dir | Present::Link(Beyond::Dir)) => Ok(Place::Inside),
        (Source::Entry, Present::Nothing) if slash => Ok(Place::Inside),
        (Source::Entry, Present::File | Present::Link(Beyond::NotDir)) if slash => Err(Conflict::SlashOnNonDir),
        (Source::Entry, _) => Ok(Place::At),
    }
}

// Checks before the first write =======================================================================================

/// The path of the tree entry at the relative path `rel` (the source root when it is empty), in the words of its side.
pub(super) type PathOf<'a> = &'a dyn Fn(&[String]) -> String;

/// Where the entries of a tree are, for the checks that run before anything is written.
pub(super) fn entries<'a>(top: &'a Kind, items: &'a [Item]) -> impl Iterator<Item = (&'a [String], &'a Kind)> {
    std::iter::once((&[][..], top)).chain(items.iter().map(|item| (item.rel.as_slice(), &item.kind)))
}

/// A line for each entry whose modification time the copy would set and cannot: a file always (it is written), a
/// directory only when the copy creates it (`created`, from [`check_clashes`]; the root is the empty path). A directory
/// that is already there keeps its own time. SFTP carries seconds within `rules.times` only, and a server may show
/// none.
pub(super) fn time_notes(
    rules: &Rules,
    top: &Kind,
    items: &[Item],
    created: &HashSet<Vec<String>>,
    path: PathOf,
) -> Vec<String> {
    entries(top, items)
        .filter(|(rel, kind)| match kind {
            Kind::File { mtime, .. } => mtime.is_none(),
            Kind::Dir { mtime } => mtime.is_none() && created.contains(*rel),
            Kind::Link { .. } => false,
        })
        .map(|(rel, _)| {
            format!(
                "the modification time of {} is not kept: it is missing or outside {}, which is all SFTP carries",
                path(rel),
                rules.times
            )
        })
        .collect()
}

/// The first symlink of the tree, if there is one.
pub(super) fn first_link(top: &Kind, items: &[Item], path: PathOf) -> Option<String> {
    entries(top, items)
        .find(|(_, kind)| matches!(kind, Kind::Link { .. }))
        .map(|(rel, _)| path(rel))
}

/// The error for a target that cannot make the symlink `link` of the source; `cause` is what it said.
pub(super) fn link_refusal(rules: &Rules, link: &str, cause: &str) -> Error {
    Error::new(
        ErrorKind::Io,
        format!(
            "cannot copy the {} symlink {link}: {} refused a test symlink ({cause})",
            rules.from, rules.to
        ),
    )
}

/// Whether the root of a merged tree starts out empty: it is missing. A symlink to a directory is followed, so what is
/// behind it is not known to be empty.
pub(super) fn root_is_fresh(present: Present) -> bool {
    present == Present::Nothing
}

/// Refuses an entry of the tree that would land on something of the wrong kind: a directory on a file, anything else
/// on a directory. Entries below one that the copy creates or replaces meet nothing, so they are not looked up.
/// `present` looks up the destination of the entry at the given relative path; `paths` spells the source and the
/// destination of that entry. Returns the directories the copy creates, the root (the empty path) among them when
/// `root_fresh`.
pub(super) fn check_clashes(
    items: &[Item],
    root_fresh: bool,
    rules: &Rules,
    present: &mut dyn FnMut(&[String]) -> Result<Present>,
    paths: &dyn Fn(&[String]) -> (String, String),
) -> Result<HashSet<Vec<String>>> {
    let mut fresh: HashSet<&[String]> = HashSet::new();
    for item in items {
        let (parent, _) = item.rel.split_at(item.rel.len() - 1);
        let parent_fresh = if parent.is_empty() {
            root_fresh
        } else {
            fresh.contains(&parent)
        };
        if parent_fresh {
            if item.kind.is_dir() {
                fresh.insert(item.rel.as_slice());
            }
            continue;
        }
        let found = present(&item.rel)?;
        let wrong = match (&item.kind, found) {
            (Kind::Dir { .. }, Present::Nothing | Present::Link(_)) => {
                fresh.insert(item.rel.as_slice());
                None
            }
            (Kind::Dir { .. }, Present::Dir) => None,
            (Kind::Dir { .. }, Present::File) => Some("which is not a directory"),
            (_, Present::Dir) => Some("which is a directory"),
            _ => None,
        };
        if let Some(wrong) = wrong {
            let (src, dest) = paths(&item.rel);
            return Err(Error::new(
                ErrorKind::Io,
                format!(
                    "cannot copy the {} {} {src} onto {dest} on {}, {wrong}",
                    rules.from,
                    item.kind.noun(),
                    rules.to
                ),
            ));
        }
    }
    let mut created: HashSet<Vec<String>> = fresh.into_iter().map(<[String]>::to_vec).collect();
    if root_fresh {
        created.insert(Vec::new());
    }
    Ok(created)
}

// Names ===============================================================================================================

/// Which names a target can store, and how it compares them.
pub(super) struct Rules<'a> {
    /// Whose names are checked: `guest` or `host`.
    pub from: &'a str,
    /// Where they are going: `this host` or `the guest`.
    pub to: &'a str,
    pub problem: &'a dyn Fn(&str) -> Option<String>,
    pub key: &'a dyn Fn(&str) -> String,
    /// The span of times that a source of this side can give SFTP, in words: `1970 to 2106`.
    pub times: &'a str,
}

/// Refuses a name of the destination the user typed that the copy would create and the target cannot store. `chain`
/// holds the destination's names from the outermost, each with the path that ends in it (`None` for a part that is no
/// name: a root, `.`, `..`). Names are looked up from the innermost, and the first that exists ends the check: the
/// target stores what it holds. A name that cannot be looked up is refusing to be, and says so less plainly; the names
/// above it are checked first, as one of them may be why, and its refusal is the plain one.
pub(super) fn check_destination<P>(
    chain: &[(Option<String>, P)],
    rules: &Rules,
    present: &mut dyn FnMut(&P) -> Result<Present>,
) -> Result<()> {
    let mut lookup_failed = None;
    for (name, path) in chain.iter().rev() {
        let Some(name) = name else { continue };
        let found = present(path);
        if matches!(found, Ok(found) if found != Present::Nothing) {
            break;
        }
        if let Some(problem) = (rules.problem)(name) {
            return Err(Error::new(
                ErrorKind::Io,
                format!(
                    "the destination name {name:?} {problem}, so it cannot be created on {}",
                    rules.to
                ),
            ));
        }
        if let Err(e) = found {
            lookup_failed.get_or_insert(e);
        }
    }
    lookup_failed.map_or(Ok(()), Err)
}

/// Refuses `name` when the target cannot hold it as one file name.
pub(super) fn check_name(name: &str, rules: &Rules) -> Result<()> {
    match (rules.problem)(name) {
        None => Ok(()),
        Some(problem) => Err(Error::new(
            ErrorKind::Io,
            format!(
                "the {} name {name:?} {problem}, so it cannot be copied to {}",
                rules.from, rules.to
            ),
        )),
    }
}

/// Refuses names the target cannot hold, and two names it takes for one (so the second would overwrite the first).
pub(super) fn check_names(items: &[Item], rules: &Rules) -> Result<()> {
    let mut seen: HashMap<Vec<String>, &[String]> = HashMap::new();
    for item in items {
        for part in &item.rel {
            check_name(part, rules)?;
        }
        let key = item.rel.iter().map(|part| (rules.key)(part)).collect();
        if let Some(first) = seen.insert(key, &item.rel)
            && first != item.rel.as_slice()
        {
            return Err(Error::new(
                ErrorKind::Io,
                format!(
                    "the {from} paths {:?} and {:?} are one name on {to}, whose file system ignores case, \
                     so copying both would overwrite the first",
                    first.join("/"),
                    item.rel.join("/"),
                    from = rules.from,
                    to = rules.to,
                ),
            ));
        }
    }
    Ok(())
}

/// Refuses a listing of `dir` whose names cannot be told apart. The library decodes names lossily, so the bytes
/// `a\xffb` and the text `a\u{fffd}b` both arrive as `a\u{fffd}b`, and a name with U+FFFD may be either. `names` is
/// sorted.
pub(super) fn check_listing(dir: &str, names: &[String]) -> Result<()> {
    if let Some(name) = names.iter().find(|name| name.contains('\u{fffd}')) {
        return Err(Error::new(
            ErrorKind::Io,
            format!(
                "the guest path {:?} has a name that is not valid UTF-8 or contains U+FFFD, which SFTP cannot \
                 tell apart",
                join(dir, name)
            ),
        ));
    }
    if let Some(pair) = names.windows(2).find(|pair| pair[0] == pair[1]) {
        return Err(Error::new(
            ErrorKind::Io,
            format!("the listing of the guest directory {dir} names {:?} twice", pair[0]),
        ));
    }
    Ok(())
}

// Guest paths =========================================================================================================

pub(super) fn join(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// The parts of the guest path `path` from the outermost, for [`check_destination`]: each with the path that ends in
/// it.
pub(super) fn chain(path: &str) -> Vec<(Option<String>, String)> {
    let parts: Vec<&str> = path.split('/').collect();
    (0..parts.len())
        .map(|i| {
            let name = (!matches!(parts[i], "" | "." | "..")).then(|| parts[i].to_owned());
            (name, parts[..=i].join("/"))
        })
        .collect()
}

pub(super) fn parent_of(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(i) => trimmed[..i].to_owned(),
        None if trimmed.is_empty() => "/".to_owned(),
        None => ".".to_owned(),
    }
}

/// The guest path `path` as the user writes it with its `.` segments dropped and each `..` cancelled against the name
/// before it, by name only (a symlink before a `..` is not followed), so a destination is created exactly as it reads.
/// A `..` at a root stays at the root, whatever `guest` calls a root (`/`, `Z:/`, `//server/share/`); one with nothing
/// before it stays; no segment left is `.`. A path the guest cannot place by name (a drive-relative one) is an error.
pub(super) fn normalize_user(path: &str, guest: &dyn GuestOs) -> Result<String> {
    guest.split_root(path).map(normalize_under)
}

fn normalize_under((root, rest): (String, &str)) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => match parts.last() {
                Some(&last) if last != ".." => {
                    parts.pop();
                }
                _ if !root.is_empty() => {}
                _ => parts.push(part),
            },
            _ => parts.push(part),
        }
    }
    match (root.is_empty(), parts.is_empty()) {
        (false, _) => format!("{root}{}", parts.join("/")),
        (true, true) => ".".to_owned(),
        (true, false) => parts.join("/"),
    }
}

/// The last name of `path`, if it has one.
pub(super) fn base_name(path: &str) -> Option<&str> {
    path.rsplit('/').find(|part| !part.is_empty())
}
