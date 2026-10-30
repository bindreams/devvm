//! SFTP file copy between the host and a guest.
//!
//! One source goes to one destination, and where it lands depends on the source and the destination only (owner,
//! 2026-10-30; the rules are [`plan::place`]):
//! - A directory *becomes* the destination: its contents merge into it, it is created with its parents when missing, and
//!   it never nests, so a rerun gives the same layout.
//! - A file goes inside a destination that is a directory or ends in `/`, and is the destination otherwise.
//! - Symlinks are copied as links, never followed; a source path that is itself a link is copied as a link unless it
//!   ends in `/`. A destination that is a link to a directory is followed, with or without `/`; one that is a link to a
//!   file counts as a file. Links met inside the merged tree are replaced. A host or guest that cannot store a link is
//!   an error naming it.
//! - Contents, the execute bit and the modification time of files are kept (through [`Host`] where the host decides).
//! - Every file and link is written to `.devvm-partial-<random>` beside its target and renamed over it (on the guest
//!   side with `posix-rename@openssh.com`), so a failure leaves no partial file at the final path, a read-only target
//!   is replaced, and a symlink at the destination is replaced, never written through.
//! - Directories the copy creates keep their modification time too, set after their contents are written; one that was
//!   already there keeps its own, since setting a time needs ownership. A time SFTP cannot carry (before 1970, after
//!   2106, or none shown; from a guest, after 2038 as well) is named in a line and the copy goes on.
//! - The whole source is walked before anything is written: a name the target cannot store (the destination's own
//!   included), an entry that would land on something of the wrong kind, and a link the target cannot make fail the
//!   copy, naming it. Nothing at the destination is deleted.
//!
//! The host side of a transfer ([`local`]) runs on the calling thread; each remote step ([`remote`]) runs on the
//! runtime, bounded by [`Inner::guarded`].

mod local;
mod plan;
mod remote;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use self::plan::{Conflict, Item, Kind, Place, Rules, Source};
use self::remote::Remote;
use crate::access::paths::{check_guest_path, expand_guest_path};
use crate::access::runtime;
use crate::access::session::Inner;
use crate::exit::Progress;
use crate::guest::GuestOs;
use crate::host::Host;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "sftp_tests.rs"]
mod sftp_tests;

const POSIX_RENAME: &str = "posix-rename@openssh.com";
/// How much of a file is read or written in one go; the SFTP library splits it into packets and keeps several in
/// flight.
const CHUNK: usize = 256 * 1024;
/// SFTP requests have no time limit of their own: a dead peer is found by the keepalive and the VM watcher. The
/// library adds this to the current time, and a sum that overflows is "no deadline".
const NO_REQUEST_LIMIT_SECS: u64 = u64::MAX;

/// An SFTP session over an SSH session.
pub(crate) struct Sftp {
    remote: Remote,
}

fn partial_name() -> String {
    let mut random = [0u8; 6];
    getrandom::fill(&mut random).expect("the OS random number generator is available");
    format!(".devvm-partial-{}", hex::encode(random))
}

/// Which end of a [`pump`] failed.
#[derive(Debug)]
enum Failed {
    Read(std::io::Error),
    Write(std::io::Error),
}

/// Copies `reader` to `writer` in chunks of [`CHUNK`]. The SFTP library reports a failed guest read or write as a
/// plain `io::Error` with no mark of whose fault it was, so the end that failed is known by where the call was.
async fn pump<R, W>(mut reader: R, mut writer: W) -> std::result::Result<(), Failed>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut chunk = vec![0u8; CHUNK];
    loop {
        let read = reader.read(&mut chunk).await.map_err(Failed::Read)?;
        if read == 0 {
            return Ok(());
        }
        writer.write_all(&chunk[..read]).await.map_err(Failed::Write)?;
    }
}

fn local_error(what: &str, path: &Path, source: std::io::Error) -> Error {
    let kind = if source.kind() == std::io::ErrorKind::NotFound {
        ErrorKind::NotFound
    } else {
        ErrorKind::Io
    };
    Error::io(kind, format!("{what} {}", path.display()), source)
}

/// What the destination being in the way means, in words.
fn conflict_error(conflict: Conflict, what: &str, side: &str, dest: &str) -> Error {
    let message = match conflict {
        Conflict::DirOntoNonDir => {
            format!("cannot copy {what} onto the {side} path {dest}, which is not a directory")
        }
        Conflict::SlashOnNonDir => format!("the {side} destination {dest} ends in `/` but is not a directory"),
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

fn source_of(top: &Kind) -> Source {
    if top.is_dir() { Source::Dir } else { Source::Entry }
}

/// `path` without a trailing `/`, a lone `/` excepted.
fn without_slash(path: &str) -> String {
    match path.trim_end_matches('/') {
        "" => path.chars().take(1).collect(),
        trimmed => trimmed.to_owned(),
    }
}

impl Sftp {
    /// Opens the SFTP subsystem twice on `inner`'s connection (the second channel carries only the rename extension).
    pub(super) fn open(inner: Arc<Inner>) -> Result<Sftp> {
        let guard = inner.clone();
        let remote = runtime::run("opening the SFTP subsystem", async move {
            guard
                .guarded("opening the SFTP subsystem", true, Remote::connect(guard.clone()))
                .await
        })??;
        Ok(Sftp { remote })
    }

    /// Runs the remote step `step` on the runtime and waits for it.
    fn call<T, F, Fut>(&self, awaiting: &str, step: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Remote) -> Fut,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let remote = self.remote.clone();
        let awaiting = awaiting.to_owned();
        let future = step(remote.clone());
        runtime::run(&awaiting.clone(), async move {
            remote.inner.guarded(&awaiting, true, future).await
        })?
    }

    /// The guest user's home directory.
    pub(crate) fn home(&self) -> Result<String> {
        self.call("asking the guest for its home directory", Remote::home)
    }

    /// The guest path as the guest's SFTP server expects it.
    fn guest_path(&self, path: &str, guest: &dyn GuestOs) -> Result<String> {
        check_guest_path(path)?;
        let home = self.home()?;
        Ok(guest.sftp_path(&expand_guest_path(path, &home), &home))
    }

    /// Copies the host file, symlink or directory `src` to the guest path `dest`.
    pub(crate) fn copy_in(
        &self,
        src: &Path,
        dest: &str,
        host: &dyn Host,
        guest: &dyn GuestOs,
        progress: &dyn Progress,
    ) -> Result<()> {
        let slash = dest.ends_with('/');
        let dest = without_slash(&self.guest_path(dest, guest)?);
        let top = local::inspect(src, host)?;
        let items = if top.is_dir() {
            local::walk(src, host)?
        } else {
            Vec::new()
        };
        let rules = Rules {
            from: "host",
            to: "the guest",
            problem: &|name| guest.file_name_problem(name),
            key: &|name| guest.name_key(name),
        };
        plan::check_names(&items, &rules)?;
        plan::check_destination(&plan::chain(&dest), &rules, &mut |path| {
            self.call(&format!("inspecting the guest path {path}"), {
                let path = path.clone();
                move |remote| remote.present(path)
            })
        })?;
        let present = self.call(&format!("inspecting the guest path {dest}"), {
            let dest = dest.clone();
            move |remote| remote.present(dest)
        })?;
        let what = format!("the host directory {}", src.display());
        let placed = plan::place(source_of(&top), slash, present)
            .map_err(|conflict| conflict_error(conflict, &what, "guest", &dest))?;
        // Where a file or link goes, and the one entry that stands for it in the checks.
        let single = match placed {
            Place::Root => None,
            Place::At => Some((dest.clone(), Vec::new())),
            Place::Inside => {
                let name = local::without_trailing_separator(src)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        Error::new(ErrorKind::Usage, format!("{} has no usable file name", src.display()))
                    })?;
                plan::check_name(&name, &rules)?;
                let entry = Item {
                    rel: vec![name.clone()],
                    kind: top.clone(),
                };
                Some((plan::join(&dest, &name), vec![entry]))
            }
        };
        let source_path = |rel: &[String]| {
            rel.iter()
                .fold(src.to_owned(), |base, part| base.join(part))
                .display()
                .to_string()
        };
        let (checked, root_fresh) = match &single {
            Some((_, entries)) => (entries, false),
            None => (&items, plan::root_is_fresh(present)),
        };
        plan::check_clashes(
            checked,
            root_fresh,
            &rules,
            &mut |rel| {
                let path = plan::join_all(&dest, rel);
                self.call(&format!("inspecting the guest path {path}"), move |remote| {
                    remote.present(path)
                })
            },
            &|rel| (source_path(rel), plan::join_all(&dest, rel)),
        )?;
        // Last: the probe makes a link, which no failing check may leave behind.
        if let Some(link) = plan::first_link(&top, &items, &source_path) {
            let refused = self.call(&format!("testing symlinks in the guest path {dest}"), {
                let dest = dest.clone();
                move |remote| remote.probe_links(dest)
            })?;
            if let Some(cause) = refused {
                return Err(plan::link_refusal(&rules, &link, &cause));
            }
        }
        let target = single.as_ref().map_or(&dest, |(target, _)| target);
        progress.message(&format!(
            "copying {} to the guest ({target}): {}",
            src.display(),
            total(&top, &items)
        ));
        for note in plan::time_notes(&top, &items, &source_path) {
            progress.message(&note);
        }
        match placed {
            Place::Root => {
                // The directories the copy made and the times they get once their contents are written. A directory
                // that was already there keeps its time: setting one needs ownership the user may not have.
                let mut made = Vec::new();
                let root_made = self.call(&format!("creating the guest directory {dest}"), {
                    let dest = dest.clone();
                    move |remote| remote.put_dir(dest, true, what)
                })?;
                if let (true, Kind::Dir { mtime: Some(seconds) }) = (root_made, &top) {
                    made.push((dest.clone(), *seconds));
                }
                for item in &items {
                    let remote_path = plan::join_all(&dest, &item.rel);
                    let local_path = item.rel.iter().fold(src.to_owned(), |base, part| base.join(part));
                    let kind = item.kind.clone();
                    let item_made = self.call(
                        &format!("copying {} to the guest path {remote_path}", local_path.display()),
                        {
                            let remote_path = remote_path.clone();
                            move |remote| remote.put_item(kind, local_path, remote_path)
                        },
                    )?;
                    if let (true, Kind::Dir { mtime: Some(seconds) }) = (item_made, &item.kind) {
                        made.push((remote_path, *seconds));
                    }
                }
                // Deepest first: a directory is listed before its children.
                for (path, seconds) in made.into_iter().rev() {
                    self.call(
                        &format!("setting the time of the guest directory {path}"),
                        move |remote| remote.set_dir_time(path, seconds),
                    )?;
                }
            }
            Place::At | Place::Inside => {
                let target = target.clone();
                let dir = plan::parent_of(&target);
                self.call(
                    &format!("creating the guest directory {dir}"),
                    move |remote| async move { remote.make_dirs(&dir).await },
                )?;
                let local_path = src.to_owned();
                self.call(
                    &format!("copying {} to the guest path {target}", src.display()),
                    move |remote| remote.put_item(top, local_path, target),
                )?;
            }
        }
        Ok(())
    }

    /// Copies the guest file, symlink or directory `src` to the host path `dest`.
    pub(crate) fn copy_out(
        &self,
        src: &str,
        dest: &Path,
        host: &dyn Host,
        guest: &dyn GuestOs,
        progress: &dyn Progress,
    ) -> Result<()> {
        let slash = local::ends_with_separator(dest);
        let dest = local::without_trailing_separator(dest);
        let follow = src.ends_with('/');
        let src = self.guest_path(src, guest)?;
        let top = self
            .call(&format!("inspecting the guest path {src}"), {
                let src = src.clone();
                move |remote| remote.inspect(src, follow)
            })?
            .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("the guest source {src} does not exist")))?;
        let items = if top.is_dir() {
            self.call(&format!("listing the guest directory {src}"), {
                let src = src.clone();
                move |remote| remote.walk(src)
            })?
        } else {
            Vec::new()
        };
        let rules = Rules {
            from: "guest",
            to: "this host",
            problem: &|name| host.file_name_problem(name),
            key: &|name| host.name_key(name),
        };
        plan::check_names(&items, &rules)?;
        plan::check_destination(&local::chain(&dest), &rules, &mut |path| local::present(path))?;
        let present = local::present(&dest)?;
        let what = format!("the guest directory {src}");
        let placed = plan::place(source_of(&top), slash, present)
            .map_err(|conflict| conflict_error(conflict, &what, "host", &dest.display().to_string()))?;
        // Where a file or link goes, and the one entry that stands for it in the checks.
        let single = match placed {
            Place::Root => None,
            Place::At => Some((dest.clone(), Vec::new())),
            Place::Inside => {
                let name = plan::base_name(&src)
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::Usage,
                            format!("the guest path {src} has no file name to copy"),
                        )
                    })?
                    .to_owned();
                plan::check_name(&name, &rules)?;
                let entry = Item {
                    rel: vec![name.clone()],
                    kind: top.clone(),
                };
                Some((dest.join(&name), vec![entry]))
            }
        };
        let source_path = |rel: &[String]| plan::join_all(&src, rel);
        let host_path = |rel: &[String]| rel.iter().fold(dest.clone(), |path, part| path.join(part));
        let (checked, root_fresh) = match &single {
            Some((_, entries)) => (entries, false),
            None => (&items, plan::root_is_fresh(present)),
        };
        plan::check_clashes(
            checked,
            root_fresh,
            &rules,
            &mut |rel| local::present(&host_path(rel)),
            &|rel| (source_path(rel), host_path(rel).display().to_string()),
        )?;
        // Last: the probe makes a link, which no failing check may leave behind.
        if let Some(link) = plan::first_link(&top, &items, &source_path)
            && let Some(cause) = local::probe_links(host, &dest)?
        {
            return Err(plan::link_refusal(&rules, &link, &cause));
        }
        let target = single.as_ref().map_or(&dest, |(target, _)| target);
        progress.message(&format!(
            "copying the guest path {src} to {}: {}",
            target.display(),
            total(&top, &items)
        ));
        for note in plan::time_notes(&top, &items, &source_path) {
            progress.message(&note);
        }
        match placed {
            Place::Root => {
                // As in `copy_in`: only a directory the copy made gets the source's time.
                let mut made = Vec::new();
                if let (true, Kind::Dir { mtime: Some(seconds) }) = (local::put_dir(&dest, true, &what)?, &top) {
                    made.push((dest.clone(), *seconds));
                }
                for item in &items {
                    let remote_path = plan::join_all(&src, &item.rel);
                    let path = host_path(&item.rel);
                    let item_made = self.put_local(host, item.kind.clone(), &remote_path, &path)?;
                    if let (true, Kind::Dir { mtime: Some(seconds) }) = (item_made, &item.kind) {
                        made.push((path, *seconds));
                    }
                }
                // Deepest first: a directory is listed before its children.
                for (path, seconds) in made.into_iter().rev() {
                    local::set_dir_time(&path, seconds)?;
                }
            }
            Place::At | Place::Inside => {
                let target = target.clone();
                if let Some(dir) = target.parent().filter(|dir| !dir.as_os_str().is_empty()) {
                    local::make_dirs(dir)?;
                }
                self.put_local(host, top, &src, &target)?;
            }
        }
        Ok(())
    }

    /// Makes the host `target` the tree entry `kind` copied from the guest path `remote`. Returns whether it made a
    /// directory.
    fn put_local(&self, host: &dyn Host, kind: Kind, remote: &str, target: &Path) -> Result<bool> {
        match kind {
            Kind::Dir { .. } => local::put_dir(target, false, &format!("the guest directory {remote}")),
            Kind::Link { target: text, to_dir } => {
                local::put_link(host, &text, to_dir, target, &format!("the guest symlink {remote}")).map(|()| false)
            }
            Kind::File { executable, mtime, .. } => {
                let temp: PathBuf = local::temp_beside(target);
                let downloaded = self.call(&format!("copying the guest file {remote} to {}", target.display()), {
                    let (remote, temp) = (remote.to_owned(), temp.clone());
                    move |remote_side| remote_side.download(remote, temp)
                });
                match downloaded {
                    Ok(()) => local::finish_file(&temp, target, executable, mtime, host).map(|()| false),
                    Err(e) => {
                        let _ = std::fs::remove_file(&temp);
                        Err(e)
                    }
                }
            }
        }
    }
}
