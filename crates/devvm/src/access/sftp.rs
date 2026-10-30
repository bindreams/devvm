//! SFTP file copy between the host and a guest, with `cp -r` / `scp -r` semantics.
//!
//! A file goes to an existing directory *inside* it, onto an existing file *over* it, and to a new name as that name
//! (the parent must exist: it is not created). A directory to a missing path is created there; to an existing directory
//! it lands inside as `<dest>/<basename>`; onto an existing file is an error. Symlinks are followed (a loop or a
//! dangling link is an error naming the path), empty directories are copied, and bytes are never translated.
//!
//! Data moves in chunks, never whole, to a temporary name `.devvm-partial-<random>` beside the destination and is then
//! renamed over it (on the guest side with `posix-rename@openssh.com`, which replaces an existing file atomically), so a
//! failure leaves no partial file at the destination path. The host and the guest decide permissions through [`Host`]:
//! a copy in makes the guest file `0755` when the source is executable on the host and `0644` otherwise; a copy out
//! calls [`Host::set_executable`] when the guest file has an execute bit.
//!
//! The host side of a transfer (what exists, symlinks, modes, the temporary file and the final rename) runs on the
//! calling thread; each remote step runs on the runtime, bounded by [`Inner::guarded`].

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::{Config, RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, OpenFlags, Packet, StatusCode};
use tokio::io::{AsyncWriteExt, BufReader, BufWriter, copy_buf};

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
/// SFTP requests have no time limit of their own: a dead peer is found by the keepalive and the VM watcher.
const NO_REQUEST_LIMIT_SECS: u64 = 10 * 365 * 24 * 3600;

/// What the guest side of a transfer holds. Cheap to clone; every remote step gets one.
#[derive(Clone)]
struct Remote {
    inner: Arc<Inner>,
    session: Arc<SftpSession>,
    /// A second SFTP channel, used only for the `posix-rename@openssh.com` extension, which the high-level session
    /// does not send.
    raw: Arc<RawSftpSession>,
}

/// An SFTP session over an SSH session.
pub(crate) struct Sftp {
    remote: Remote,
}

// Planning ============================================================================================================

enum Kind {
    Dir,
    /// `executable`: the source has an execute bit.
    File {
        executable: bool,
    },
}

/// One entry of a directory tree to copy, relative to its root.
struct Item {
    rel: Vec<String>,
    kind: Kind,
}

fn join(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(i) => path[..i].to_owned(),
        None => ".".to_owned(),
    }
}

fn base_name(path: &str) -> Option<&str> {
    path.rsplit('/').find(|part| !part.is_empty())
}

fn partial_name() -> String {
    let mut random = [0u8; 6];
    getrandom::fill(&mut random).expect("the OS random number generator is available");
    format!(".devvm-partial-{}", hex::encode(random))
}

fn local_error(what: &str, path: &Path, source: std::io::Error) -> Error {
    let kind = if source.kind() == std::io::ErrorKind::NotFound {
        ErrorKind::NotFound
    } else {
        ErrorKind::Io
    };
    Error::io(kind, format!("{what} {}", path.display()), source)
}

/// The host tree below `dir`, parents before children; symlinks are followed.
fn walk_host(dir: &Path, host: &dyn Host) -> Result<Vec<Item>> {
    struct Frame {
        path: PathBuf,
        rel: Vec<String>,
        ancestors: Vec<PathBuf>,
    }
    let canonical = |path: &Path| std::fs::canonicalize(path).map_err(|e| local_error("cannot resolve", path, e));
    let mut items = Vec::new();
    let mut pending = vec![Frame {
        path: dir.to_owned(),
        rel: Vec::new(),
        ancestors: Vec::new(),
    }];
    while let Some(frame) = pending.pop() {
        let here = canonical(&frame.path)?;
        if frame.ancestors.contains(&here) {
            return Err(Error::new(
                ErrorKind::Io,
                format!(
                    "{} is a symlink loop: it leads back to a directory that contains it",
                    frame.path.display()
                ),
            ));
        }
        let mut ancestors = frame.ancestors.clone();
        ancestors.push(here);
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&frame.path).map_err(|e| local_error("cannot list", &frame.path, e))? {
            let entry = entry.map_err(|e| local_error("cannot list", &frame.path, e))?;
            names.push(entry.file_name());
        }
        names.sort();
        // Pushed in reverse so the first name is handled first; a directory's own item is added when its frame is.
        for name in names.into_iter().rev() {
            let path = frame.path.join(&name);
            let Some(text) = name.to_str() else {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!(
                        "{} has a name that is not valid Unicode, which SFTP cannot carry",
                        path.display()
                    ),
                ));
            };
            let mut rel = frame.rel.clone();
            rel.push(text.to_owned());
            let metadata = std::fs::metadata(&path).map_err(|e| {
                let why = if e.kind() == std::io::ErrorKind::NotFound {
                    format!("{} is a dangling symlink", path.display())
                } else {
                    format!("cannot read {}", path.display())
                };
                Error::io(ErrorKind::Io, why, e)
            })?;
            if metadata.is_dir() {
                items.push(Item {
                    rel: rel.clone(),
                    kind: Kind::Dir,
                });
                pending.push(Frame {
                    path,
                    rel,
                    ancestors: ancestors.clone(),
                });
            } else if metadata.is_file() {
                let executable = host
                    .is_executable(&path)
                    .map_err(|e| local_error("cannot read the mode of", &path, e))?;
                items.push(Item {
                    rel,
                    kind: Kind::File { executable },
                });
            } else {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("{} is neither a regular file nor a directory", path.display()),
                ));
            }
        }
    }
    Ok(items)
}

// Remote steps ========================================================================================================

impl Remote {
    fn error(&self, awaiting: &str, e: SftpError) -> Error {
        match e {
            SftpError::Status(status) if status.status_code == StatusCode::NoSuchFile => Error::new(
                ErrorKind::NotFound,
                format!(
                    "{awaiting}: the guest has no such file or directory ({})",
                    status.error_message
                ),
            ),
            SftpError::Status(status) => Error::new(
                ErrorKind::Io,
                format!(
                    "{awaiting}: the guest said: {}: {}",
                    status.status_code, status.error_message
                ),
            ),
            other => self.inner.connection_ended(&format!("{awaiting} ({other})")),
        }
    }

    /// An `io::Error` out of a copy loop: the guest's side if it carries an SFTP error, else the host's.
    fn copy_error(&self, awaiting: &str, local: &Path, e: std::io::Error) -> Error {
        match e.get_ref().and_then(|inner| inner.downcast_ref::<SftpError>()) {
            Some(remote) => self.error(awaiting, remote.clone()),
            None => local_error("cannot copy", local, e),
        }
    }

    /// The attributes of `path`, following symlinks; `None` when it does not exist.
    async fn stat(&self, path: &str) -> Result<Option<FileAttributes>> {
        match self.session.metadata(path).await {
            Ok(attrs) => Ok(Some(attrs)),
            Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => Ok(None),
            Err(e) => Err(self.error(&format!("cannot inspect the guest path {path}"), e)),
        }
    }

    async fn ensure_dir(&self, path: &str) -> Result<()> {
        match self.stat(path).await? {
            Some(attrs) if attrs.is_dir() => Ok(()),
            Some(_) => Err(Error::new(
                ErrorKind::Io,
                format!("the guest path {path} exists and is not a directory"),
            )),
            None => self
                .session
                .create_dir(path)
                .await
                .map_err(|e| self.error(&format!("cannot create the guest directory {path}"), e)),
        }
    }

    async fn posix_rename(&self, from: &str, to: &str) -> Result<()> {
        let mut data = Vec::new();
        for text in [from, to] {
            data.extend_from_slice(&(text.len() as u32).to_be_bytes());
            data.extend_from_slice(text.as_bytes());
        }
        let awaiting = format!("cannot move {from} to {to} on the guest");
        match self.raw.extended(POSIX_RENAME, data).await {
            Ok(Packet::Status(status)) if status.status_code == StatusCode::Ok => Ok(()),
            Ok(Packet::Status(status)) => Err(self.error(&awaiting, SftpError::Status(status))),
            Ok(_) => Err(Error::new(ErrorKind::Io, format!("{awaiting}: unexpected reply"))),
            Err(e) => Err(self.error(&awaiting, e)),
        }
    }

    /// Streams `local` to the guest file `target`, through a temporary file renamed over `target`.
    async fn upload(self, local: PathBuf, target: String, executable: bool) -> Result<()> {
        if self.stat(&target).await?.is_some_and(|attrs| attrs.is_dir()) {
            return Err(Error::new(
                ErrorKind::Io,
                format!("cannot copy {} onto the guest directory {target}", local.display()),
            ));
        }
        let temp = join(&parent_of(&target), &partial_name());
        let copied = self.upload_to(&local, &temp, executable).await;
        let moved = match copied {
            Ok(()) => self.posix_rename(&temp, &target).await,
            Err(e) => Err(e),
        };
        if moved.is_err() {
            let _ = self.session.remove_file(temp).await;
        }
        moved
    }

    async fn upload_to(&self, local: &Path, temp: &str, executable: bool) -> Result<()> {
        let awaiting = format!("cannot write the guest file {temp}");
        let attributes = FileAttributes {
            permissions: Some(if executable { 0o755 } else { 0o644 }),
            ..FileAttributes::default()
        };
        let mut file = self
            .session
            .open_with_flags_and_attributes(
                temp,
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
                attributes,
            )
            .await
            .map_err(|e| self.error(&awaiting, e))?;
        let source = tokio::fs::File::open(local)
            .await
            .map_err(|e| local_error("cannot read", local, e))?;
        copy_buf(&mut BufReader::with_capacity(CHUNK, source), &mut file)
            .await
            .map_err(|e| self.copy_error(&awaiting, local, e))?;
        // Shutting down flushes and closes the handle: a failed write may only show here.
        file.shutdown().await.map_err(|e| self.copy_error(&awaiting, local, e))
    }

    /// Streams the guest file `remote` into the new host file `temp`.
    async fn download(self, remote: String, temp: PathBuf) -> Result<()> {
        let awaiting = format!("cannot read the guest file {remote}");
        let file = self
            .session
            .open(remote.as_str())
            .await
            .map_err(|e| self.error(&awaiting, e))?;
        let local = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await
            .map_err(|e| local_error("cannot create", &temp, e))?;
        let mut writer = BufWriter::with_capacity(CHUNK, local);
        copy_buf(&mut BufReader::with_capacity(CHUNK, file), &mut writer)
            .await
            .map_err(|e| self.copy_error(&awaiting, &temp, e))?;
        writer
            .shutdown()
            .await
            .map_err(|e| local_error("cannot finish", &temp, e))
    }

    /// The guest tree below `dir`, parents before children; symlinks are followed.
    async fn walk(self, dir: String) -> Result<Vec<Item>> {
        struct Frame {
            path: String,
            rel: Vec<String>,
            ancestors: Vec<String>,
        }
        let mut items = Vec::new();
        let mut pending = vec![Frame {
            path: dir,
            rel: Vec::new(),
            ancestors: Vec::new(),
        }];
        while let Some(frame) = pending.pop() {
            let awaiting = format!("cannot list the guest directory {}", frame.path);
            let here = self
                .session
                .canonicalize(frame.path.as_str())
                .await
                .map_err(|e| self.error(&awaiting, e))?;
            if frame.ancestors.contains(&here) {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!(
                        "the guest path {} is a symlink loop: it leads back to a directory that contains it",
                        frame.path
                    ),
                ));
            }
            let mut ancestors = frame.ancestors.clone();
            ancestors.push(here);
            let listing = self
                .session
                .read_dir(frame.path.as_str())
                .await
                .map_err(|e| self.error(&awaiting, e))?;
            let mut entries: Vec<_> = listing.map(|entry| entry.file_name()).collect();
            entries.sort();
            for name in entries.into_iter().rev() {
                let path = join(&frame.path, &name);
                let mut rel = frame.rel.clone();
                rel.push(name);
                // `stat`, not the listing's attributes: those describe a symlink, not what it points to.
                let Some(attrs) = self.stat(&path).await? else {
                    return Err(Error::new(
                        ErrorKind::Io,
                        format!("the guest path {path} is a dangling symlink"),
                    ));
                };
                if attrs.is_dir() {
                    items.push(Item {
                        rel: rel.clone(),
                        kind: Kind::Dir,
                    });
                    pending.push(Frame {
                        path,
                        rel,
                        ancestors: ancestors.clone(),
                    });
                } else if attrs.file_type().is_file() {
                    items.push(Item {
                        rel,
                        kind: Kind::File {
                            executable: attrs.permissions.is_some_and(|mode| mode & 0o111 != 0),
                        },
                    });
                } else {
                    return Err(Error::new(
                        ErrorKind::Io,
                        format!("the guest path {path} is neither a regular file nor a directory"),
                    ));
                }
            }
        }
        Ok(items)
    }
}

// The blocking API ====================================================================================================

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
        self.call("asking the guest for its home directory", |remote| async move {
            remote
                .session
                .canonicalize(".")
                .await
                .map_err(|e| remote.error("cannot find the guest home directory", e))
        })
    }

    /// The guest path as the guest's SFTP server expects it.
    fn guest_path(&self, path: &str, guest: &dyn GuestOs) -> Result<String> {
        check_guest_path(path)?;
        let home = self.home()?;
        Ok(guest.sftp_path(&expand_guest_path(path, &home), &home))
    }

    /// Copies the host file or directory `src` to the guest path `dest`.
    pub(crate) fn copy_in(
        &self,
        src: &Path,
        dest: &str,
        host: &dyn Host,
        guest: &dyn GuestOs,
        progress: &dyn Progress,
    ) -> Result<()> {
        let dest = self.guest_path(dest, guest)?;
        let metadata = std::fs::metadata(src).map_err(|e| local_error("the host source", src, e))?;
        progress.message(&format!("copying {} to the guest ({dest})", src.display()));
        let existing = self.call(&format!("inspecting the guest path {dest}"), {
            let dest = dest.clone();
            move |remote| async move { remote.stat(&dest).await }
        })?;
        let name = || {
            src.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
                .ok_or_else(|| Error::new(ErrorKind::Usage, format!("{} has no usable file name", src.display())))
        };
        let missing_parent = |remote_dest: &str| -> Result<()> {
            let parent = parent_of(remote_dest);
            let parent_attrs = self.call(&format!("inspecting the guest path {parent}"), {
                let parent = parent.clone();
                move |remote| async move { remote.stat(&parent).await }
            })?;
            match parent_attrs {
                Some(attrs) if attrs.is_dir() => Ok(()),
                Some(_) => Err(Error::new(
                    ErrorKind::Io,
                    format!("the guest destination parent {parent} is not a directory"),
                )),
                None => Err(Error::new(
                    ErrorKind::NotFound,
                    format!("the guest destination directory {parent} does not exist"),
                )),
            }
        };

        if metadata.is_file() {
            let target = match &existing {
                Some(attrs) if attrs.is_dir() => join(&dest, &name()?),
                Some(_) => dest.clone(),
                None => {
                    missing_parent(&dest)?;
                    dest.clone()
                }
            };
            let executable = host
                .is_executable(src)
                .map_err(|e| local_error("cannot read the mode of", src, e))?;
            let local = src.to_owned();
            return self.call(
                &format!("copying {} to the guest path {target}", src.display()),
                move |remote| remote.upload(local, target, executable),
            );
        }
        if !metadata.is_dir() {
            return Err(Error::new(
                ErrorKind::Io,
                format!("{} is neither a regular file nor a directory", src.display()),
            ));
        }
        let root = match &existing {
            Some(attrs) if attrs.is_dir() => join(&dest, &name()?),
            Some(_) => {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!("cannot copy the directory {} onto the guest file {dest}", src.display()),
                ));
            }
            None => {
                missing_parent(&dest)?;
                dest.clone()
            }
        };
        let items = walk_host(src, host)?;
        self.call(&format!("creating the guest directory {root}"), {
            let root = root.clone();
            move |remote| async move { remote.ensure_dir(&root).await }
        })?;
        for item in items {
            let remote_path = item.rel.iter().fold(root.clone(), |base, part| join(&base, part));
            let local = item.rel.iter().fold(src.to_owned(), |base, part| base.join(part));
            match item.kind {
                Kind::Dir => self.call(
                    &format!("creating the guest directory {remote_path}"),
                    move |remote| async move { remote.ensure_dir(&remote_path).await },
                )?,
                Kind::File { executable } => self.call(
                    &format!("copying {} to the guest path {remote_path}", local.display()),
                    move |remote| remote.upload(local, remote_path, executable),
                )?,
            }
        }
        Ok(())
    }

    /// Copies the guest file or directory `src` to the host path `dest`.
    pub(crate) fn copy_out(
        &self,
        src: &str,
        dest: &Path,
        host: &dyn Host,
        guest: &dyn GuestOs,
        progress: &dyn Progress,
    ) -> Result<()> {
        let src = self.guest_path(src, guest)?;
        let attrs = self
            .call(&format!("inspecting the guest path {src}"), {
                let src = src.clone();
                move |remote| async move { remote.stat(&src).await }
            })?
            .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("the guest source {src} does not exist")))?;
        progress.message(&format!("copying the guest path {src} to {}", dest.display()));
        let existing = match std::fs::metadata(dest) {
            Ok(metadata) => Some(metadata),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(local_error("the host destination", dest, e)),
        };
        let name = || {
            base_name(&src).map(str::to_owned).ok_or_else(|| {
                Error::new(
                    ErrorKind::Usage,
                    format!("the guest path {src} has no file name to copy"),
                )
            })
        };
        let check_parent = |path: &Path| -> Result<()> {
            let parent = match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                _ => Path::new("."),
            };
            if parent.is_dir() {
                Ok(())
            } else {
                Err(Error::new(
                    ErrorKind::NotFound,
                    format!("the host destination directory {} does not exist", parent.display()),
                ))
            }
        };

        if attrs.file_type().is_file() {
            let target = match &existing {
                Some(metadata) if metadata.is_dir() => dest.join(name()?),
                Some(_) => dest.to_owned(),
                None => {
                    check_parent(dest)?;
                    dest.to_owned()
                }
            };
            return self.fetch(
                &src,
                &target,
                attrs.permissions.is_some_and(|mode| mode & 0o111 != 0),
                host,
            );
        }
        let root = match &existing {
            Some(metadata) if metadata.is_dir() => dest.join(name()?),
            Some(_) => {
                return Err(Error::new(
                    ErrorKind::Io,
                    format!(
                        "cannot copy the guest directory {src} onto the host file {}",
                        dest.display()
                    ),
                ));
            }
            None => {
                check_parent(dest)?;
                dest.to_owned()
            }
        };
        let items = self.call(&format!("listing the guest directory {src}"), {
            let src = src.clone();
            move |remote| async move { remote.walk(src).await }
        })?;
        ensure_host_dir(&root)?;
        for item in items {
            let remote_path = item.rel.iter().fold(src.clone(), |base, part| join(&base, part));
            let local = item.rel.iter().fold(root.clone(), |base, part| base.join(part));
            match item.kind {
                Kind::Dir => ensure_host_dir(&local)?,
                Kind::File { executable } => self.fetch(&remote_path, &local, executable, host)?,
            }
        }
        Ok(())
    }

    /// Downloads the guest file `remote` to `target` through a temporary file beside it.
    fn fetch(&self, remote: &str, target: &Path, executable: bool, host: &dyn Host) -> Result<()> {
        if target.is_dir() {
            return Err(Error::new(
                ErrorKind::Io,
                format!(
                    "cannot copy the guest file {remote} onto the host directory {}",
                    target.display()
                ),
            ));
        }
        let parent = match target.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        let temp = parent.join(partial_name());
        let downloaded = self.call(&format!("copying the guest file {remote} to {}", target.display()), {
            let (remote, temp) = (remote.to_owned(), temp.clone());
            move |remote_side| remote_side.download(remote, temp)
        });
        let finished = downloaded.and_then(|()| {
            std::fs::rename(&temp, target).map_err(|e| local_error("cannot move the copy into place at", target, e))
        });
        match finished {
            Ok(()) => {
                if executable {
                    host.set_executable(target)
                        .map_err(|e| local_error("cannot make executable", target, e))?;
                }
                Ok(())
            }
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                Err(e)
            }
        }
    }
}

impl Remote {
    async fn connect(inner: Arc<Inner>) -> Result<Remote> {
        let config = Config {
            request_timeout_secs: NO_REQUEST_LIMIT_SECS,
            ..Config::default()
        };
        let stream = |inner: Arc<Inner>| async move {
            let channel = inner
                .handle
                .channel_open_session()
                .await
                .map_err(|_| inner.connection_ended("opening the SFTP subsystem"))?;
            channel
                .request_subsystem(true, "sftp")
                .await
                .map_err(|_| inner.connection_ended("opening the SFTP subsystem"))?;
            Ok::<_, Error>(channel.into_stream())
        };
        let session = SftpSession::new_with_config(stream(inner.clone()).await?, config.clone())
            .await
            .map_err(|e| Remote::start_error(&inner, e))?;
        let raw = RawSftpSession::new_with_config(stream(inner.clone()).await?, config);
        raw.init().await.map_err(|e| Remote::start_error(&inner, e))?;
        Ok(Remote {
            inner,
            session: Arc::new(session),
            raw: Arc::new(raw),
        })
    }

    fn start_error(inner: &Inner, e: SftpError) -> Error {
        inner.connection_ended(&format!("starting the SFTP subsystem ({e})"))
    }
}

fn ensure_host_dir(path: &Path) -> Result<()> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(Error::new(
            ErrorKind::Io,
            format!("{} exists and is not a directory", path.display()),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(path).map_err(|e| local_error("cannot create the directory", path, e))
        }
        Err(e) => Err(local_error("cannot inspect", path, e)),
    }
}
