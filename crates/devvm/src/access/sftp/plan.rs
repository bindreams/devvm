//! The decisions of a copy that do not depend on which side is the host: where the source lands, and which names the
//! target cannot store. Both directions share them, so the rules are written once.

use std::collections::{HashMap, HashSet};

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

    fn noun(&self) -> &'static str {
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
    /// The destination becomes the directory and the contents merge into it. `follow`: a symlink at the destination is
    /// followed (the user wrote a trailing `/`), not replaced.
    Root { follow: bool },
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
/// never depends on anything but these three, so a rerun lands where the first run did.
pub(super) fn place(source: Source, slash: bool, dest: Present) -> std::result::Result<Place, Conflict> {
    match (source, dest) {
        (Source::Dir, Present::Nothing | Present::Dir) => Ok(Place::Root { follow: false }),
        (Source::Dir, Present::File) => Err(Conflict::DirOntoNonDir),
        (Source::Dir, Present::Link(Beyond::Dir)) => Ok(Place::Root { follow: slash }),
        (Source::Dir, Present::Link(Beyond::NotDir)) if slash => Err(Conflict::DirOntoNonDir),
        (Source::Dir, Present::Link(Beyond::NotDir)) => Ok(Place::Root { follow: false }),
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

/// A line for each entry whose modification time is not kept: SFTP carries seconds from 1970 to 2106 only, and a
/// server may show none.
pub(super) fn time_notes(top: &Kind, items: &[Item], path: PathOf) -> Vec<String> {
    entries(top, items)
        .filter(|(_, kind)| matches!(kind, Kind::File { mtime: None, .. } | Kind::Dir { mtime: None }))
        .map(|(rel, _)| {
            format!(
                "the modification time of {} is not kept: it is missing or outside 1970 to 2106, which SFTP carries",
                path(rel)
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
            "cannot copy the {} symlink {link}: {} cannot make symlinks ({cause})",
            rules.from, rules.to
        ),
    )
}

/// Whether the root of a merged tree starts out empty: missing, or a symlink that the copy replaces.
pub(super) fn root_is_fresh(present: Present, follow: bool) -> bool {
    match present {
        Present::Nothing => true,
        Present::Link(_) => !follow,
        Present::Dir | Present::File => false,
    }
}

/// Refuses an entry of the tree that would land on something of the wrong kind: a directory on a file, anything else
/// on a directory. Entries below one that the copy creates or replaces meet nothing, so they are not looked up.
/// `present` looks up the destination of the entry at the given relative path; `paths` spells the source and the
/// destination of that entry.
pub(super) fn check_clashes(
    items: &[Item],
    root_fresh: bool,
    rules: &Rules,
    present: &mut dyn FnMut(&[String]) -> Result<Present>,
    paths: &dyn Fn(&[String]) -> (String, String),
) -> Result<()> {
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
    Ok(())
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
                "the guest path {:?} has a name that is not valid UTF-8 or contains U+FFFD, which SFTP cannot tell apart",
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

/// `base` below which `rel` lies.
pub(super) fn join_all(base: &str, rel: &[String]) -> String {
    rel.iter().fold(base.to_owned(), |path, part| join(&path, part))
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

/// The last name of `path`, if it has one.
pub(super) fn base_name(path: &str) -> Option<&str> {
    path.rsplit('/').find(|part| !part.is_empty())
}
