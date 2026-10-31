//! The copy algorithm, written once for both directions. A [`CopySide`] is one end of a copy, the host or the guest;
//! [`copy`] reads the source side, checks everything against the destination side, and only then writes. Every rule of
//! the copy is in [`plan`] or here; a side supplies how its own file system is read and written, and its wording.

use super::plan::{self, Beyond, Conflict, Item, Kind, Place, Present, Rules, Source};
use crate::exit::Progress;
use crate::{Error, ErrorKind, Result};

/// One end of a copy.
pub(super) trait CopySide {
    /// A path on this side, in its own spelling.
    type Path: Clone;

    /// Whose paths and names these are in messages: `host` or `guest`.
    const NAME: &'static str;
    /// This side as the place a copy goes to, in a sentence: `this host` or `the guest`.
    const AS_DESTINATION: &'static str;
    /// The span of modification times that a source on this side can give SFTP, in words.
    const SOURCE_TIMES: &'static str;

    /// `path` in the words of a message.
    fn show(&self, path: &Self::Path) -> String;
    /// The entry `name` of the directory `base`.
    fn join(&self, base: &Self::Path, name: &str) -> Self::Path;
    /// The directory that holds `path`; `None` when there is none to make.
    fn parent(&self, path: &Self::Path) -> Option<Self::Path>;
    /// The last name of `path`, for a source that goes inside a directory; a usage error when it has none.
    fn file_name(&self, path: &Self::Path) -> Result<String>;
    /// The parts of `path` from the outermost, for [`plan::check_destination`]: each with the path that ends in it.
    fn chain(&self, path: &Self::Path) -> Vec<(Option<String>, Self::Path)>;

    /// Why `name` cannot be one file name in a directory on this side; `None` when it can.
    fn file_name_problem(&self, name: &str) -> Option<String>;
    /// `name` as this side's file system compares names.
    fn name_key(&self, name: &str) -> String;

    // As a source -----------------------------------------------------------------------------------------------------

    /// The source `path`: a symlink is a link unless `follow`. A path that does not exist is an error.
    fn inspect(&self, path: &Self::Path, follow: bool) -> Result<Kind>;
    /// The tree below the directory `path`, parents before children, symlinks as links.
    fn walk(&self, path: &Self::Path) -> Result<Vec<Item>>;

    // As a destination ------------------------------------------------------------------------------------------------

    /// What `path` is. A symlink is reported as leading to a directory only when `follow` is set, which looks where
    /// it leads (and fails when that cannot be told); without it, every link is `Link(Beyond::NotDir)`, for a link that
    /// the copy replaces whatever is behind it.
    fn present(&self, path: &Self::Path, follow: bool) -> Result<Present>;
    /// Makes the directory `path`, and its missing parents first. Returns whether it made `path`: `false` when a
    /// directory was there already, however it came to be (a link to one counts). Something else there is an error.
    fn create_dir(&self, path: &Self::Path) -> Result<bool>;
    /// Removes the symlink `path` (not what it leads to).
    fn remove_link(&self, path: &Self::Path) -> Result<()>;
    /// Makes `path` a symlink holding `text` in place of whatever is there. `what` names the source entry.
    fn put_link(&self, text: &str, to_dir: bool, path: &Self::Path, what: &str) -> Result<()>;
    /// Sets the modification time of the directory `path`, once its contents are written.
    fn set_dir_time(&self, path: &Self::Path, seconds: u32) -> Result<()>;
    /// Whether symlinks can be made where a copy to `dest` would: `None` when they can, else what was said. The probe
    /// is a link made and removed in the nearest directory that exists.
    fn probe_links(&self, dest: &Self::Path) -> Result<Option<String>>;

    /// `rel` below `base`.
    fn join_all(&self, base: &Self::Path, rel: &[String]) -> Self::Path {
        rel.iter().fold(base.clone(), |path, part| self.join(&path, part))
    }
}

/// A [`CopySide`] that can take files from the side `S`. The two directions move bytes differently, and each reports
/// its own steps.
pub(super) trait Receives<S: CopySide>: CopySide {
    /// Writes the file `from` of `source` to `to` through a temporary file, with the given execute bit and time.
    fn put_file(&self, source: &S, from: &S::Path, executable: bool, mtime: Option<u32>, to: &Self::Path)
    -> Result<()>;

    /// The line that announces a copy of `from` to `to`; `total` is what the walk found.
    fn announce(&self, from: &str, to: &str, total: &str) -> String;
}

/// What the destination being in the way means, in words.
fn conflict_error(conflict: Conflict, what: &str, side: &str, dest: &str) -> Error {
    let message = match conflict {
        Conflict::DirOntoNonDir => {
            format!("cannot copy {what} onto the {side} path {dest}, which is not a directory")
        }
        Conflict::SlashOnNonDir => {
            format!("cannot copy {what} to the {side} destination {dest}: it ends in `/` but is not a directory")
        }
    };
    Error::new(ErrorKind::Io, message)
}

/// The announcement of a copy: what the walk found.
fn total(top: &Kind, items: &[Item]) -> String {
    let (mut files, mut dirs, mut links, mut bytes) = (0u64, 0u64, 0u64, 0u64);
    for kind in std::iter::once(top).chain(items.iter().map(|item| &item.kind)) {
        match kind {
            Kind::Dir { .. } => dirs += 1,
            Kind::File { size, .. } => {
                files += 1;
                bytes += size;
            }
            Kind::Link { .. } => links += 1,
        }
    }
    format!(
        "{files} files ({}), {links} symlinks, {} directories",
        bytesize::ByteSize(bytes),
        dirs.saturating_sub(u64::from(top.is_dir()))
    )
}

/// Makes `path` a directory for the tree entry `what`: a missing one is made with its parents, a symlink is replaced by
/// a directory (or followed, when `follow`), a file is an error. Returns whether it made the directory.
///
/// The directory is made by one call that fails if it exists, and a call that found it there is looked at again, so
/// a directory or a link that appears between the look and the call is neither counted as made nor written through.
fn put_dir<D: CopySide>(dst: &D, path: &D::Path, follow: bool, what: &str) -> Result<bool> {
    loop {
        match dst.present(path, follow)? {
            Present::Dir => return Ok(false),
            Present::Link(Beyond::Dir) if follow => return Ok(false),
            Present::Link(_) if !follow => dst.remove_link(path)?,
            Present::Nothing => {}
            Present::File | Present::Link(_) => {
                return Err(conflict_error(Conflict::DirOntoNonDir, what, D::NAME, &dst.show(path)));
            }
        }
        if dst.create_dir(path)? {
            return Ok(true);
        }
    }
}

/// Makes `to` the tree entry `kind` copied from `from`. Returns whether it made a directory.
fn put_item<S, D>(src: &S, dst: &D, kind: &Kind, from: &S::Path, to: &D::Path) -> Result<bool>
where
    S: CopySide,
    D: Receives<S>,
{
    let what = format!("the {} {} {}", S::NAME, kind.noun(), src.show(from));
    match kind {
        Kind::Dir { .. } => put_dir(dst, to, false, &what),
        Kind::Link { target, to_dir } => dst.put_link(target, *to_dir, to, &what).map(|()| false),
        Kind::File { executable, mtime, .. } => dst.put_file(src, from, *executable, *mtime, to).map(|()| false),
    }
}

/// Copies the file, symlink or directory `from` of `src` (a symlink is followed when `follow`) to the destination
/// `dest` of `dst`, written with a trailing `/` when `slash`. `dest` has no trailing `/` and no `.` or `..` segment.
pub(super) fn copy<S, D>(
    src: &S,
    from: &S::Path,
    follow: bool,
    dst: &D,
    dest: &D::Path,
    slash: bool,
    progress: &dyn Progress,
) -> Result<()>
where
    S: CopySide,
    D: Receives<S>,
{
    let top = src.inspect(from, follow)?;
    let items = if top.is_dir() { src.walk(from)? } else { Vec::new() };
    let rules = Rules {
        from: S::NAME,
        to: D::AS_DESTINATION,
        problem: &|name| dst.file_name_problem(name),
        key: &|name| dst.name_key(name),
        times: S::SOURCE_TIMES,
    };
    plan::check_names(&items, &rules)?;
    plan::check_destination(&dst.chain(dest), &rules, &mut |path| dst.present(path, false))?;
    let present = dst.present(dest, true)?;
    let what = format!("the {} {} {}", S::NAME, top.noun(), src.show(from));
    let source = if top.is_dir() { Source::Dir } else { Source::Entry };
    let placed = plan::place(source, slash, present)
        .map_err(|conflict| conflict_error(conflict, &what, D::NAME, &dst.show(dest)))?;
    // Where a file or link goes, and the one entry that stands for it in the checks.
    let single = match placed {
        Place::Root => None,
        Place::At => Some((dest.clone(), Vec::new())),
        Place::Inside => {
            let name = src.file_name(from)?;
            plan::check_name(&name, &rules)?;
            let entry = Item {
                rel: vec![name.clone()],
                kind: top.clone(),
            };
            Some((dst.join(dest, &name), vec![entry]))
        }
    };
    let source_path = |rel: &[String]| src.show(&src.join_all(from, rel));
    let dest_path = |rel: &[String]| dst.join_all(dest, rel);
    let (checked, root_fresh) = match &single {
        Some((_, entries)) => (entries, false),
        None => (&items, plan::root_is_fresh(present)),
    };
    let created = plan::check_clashes(
        checked,
        root_fresh,
        &rules,
        &mut |rel| dst.present(&dest_path(rel), false),
        &|rel| (source_path(rel), dst.show(&dest_path(rel))),
    )?;
    // Last: the probe makes a link, which no failing check may leave behind.
    if let Some(link) = plan::first_link(&top, &items, &source_path)
        && let Some(cause) = dst.probe_links(dest)?
    {
        return Err(plan::link_refusal(&rules, &link, &cause));
    }
    let target = single.as_ref().map_or(dest, |(target, _)| target);
    progress.message(&dst.announce(&src.show(from), &dst.show(target), &total(&top, &items)));
    for note in plan::time_notes(&rules, &top, &items, &created, &source_path) {
        progress.message(&note);
    }
    match placed {
        Place::Root => {
            // The directories the copy made and the times they get once their contents are written. A directory that
            // was already there keeps its time: setting one needs ownership the user may not have.
            let mut made: Vec<(D::Path, u32)> = Vec::new();
            let root_made = put_dir(dst, dest, true, &what)?;
            if let (true, Kind::Dir { mtime: Some(seconds) }) = (root_made, &top) {
                made.push((dest.clone(), *seconds));
            }
            for item in &items {
                let to = dest_path(&item.rel);
                let item_made = put_item(src, dst, &item.kind, &src.join_all(from, &item.rel), &to)?;
                if let (true, Kind::Dir { mtime: Some(seconds) }) = (item_made, &item.kind) {
                    made.push((to, *seconds));
                }
            }
            // Deepest first: a directory is listed before its children.
            for (path, seconds) in made.into_iter().rev() {
                dst.set_dir_time(&path, seconds)?;
            }
        }
        Place::At | Place::Inside => {
            if let Some(dir) = dst.parent(target) {
                dst.create_dir(&dir)?;
            }
            put_item(src, dst, &top, from, target)?;
        }
    }
    Ok(())
}
