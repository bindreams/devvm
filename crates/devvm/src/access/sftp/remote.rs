//! The guest side of a copy: what the guest's SFTP server is asked, step by step. Each step is one future that runs on
//! the runtime; [`super::Sftp`] drives them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::{Config, RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags, Packet, StatusCode};
use tokio::io::AsyncWriteExt;

use super::plan::{Beyond, Item, Kind, Present, check_listing, join, parent_of};
use super::{Failed, NO_REQUEST_LIMIT_SECS, POSIX_RENAME, local_error, partial_name, pump};
use crate::access::session::Inner;
use crate::{Error, ErrorKind, Result};

/// What asking the guest for one directory came to.
enum Mkdir {
    Made,
    /// A directory was there already.
    Existed,
    /// The guest said no, and nothing is there.
    Refused(Error),
}

/// What the guest side of a transfer holds. Cheap to clone; every remote step gets one.
#[derive(Clone)]
pub(super) struct Remote {
    pub(super) inner: Arc<Inner>,
    pub(super) session: Arc<SftpSession>,
    /// A second SFTP channel, used only for the `posix-rename@openssh.com` extension, which the high-level session
    /// does not send.
    raw: Arc<RawSftpSession>,
}

impl Remote {
    pub(super) async fn connect(inner: Arc<Inner>) -> Result<Remote> {
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
        sftp_error(inner, "starting the SFTP subsystem", e)
    }

    fn error(&self, awaiting: &str, e: SftpError) -> Error {
        sftp_error(&self.inner, awaiting, e)
    }

    /// An `io::Error` out of a guest file read, write or close.
    fn guest_io(&self, awaiting: &str, e: std::io::Error) -> Error {
        match e.get_ref().and_then(|inner| inner.downcast_ref::<SftpError>()) {
            Some(remote) => self.error(awaiting, remote.clone()),
            None => Error::io(ErrorKind::Io, format!("{awaiting}: the guest side failed: {e}"), e),
        }
    }
}

/// `e`, a failure of the SFTP library, as an error of the operation `awaiting`. A status is what the guest said. Any
/// other failure is a lost connection only if the connection has ended; otherwise the SFTP channel or the server on it
/// failed and the connection lives, which is not the same to the caller.
fn sftp_error(inner: &Inner, awaiting: &str, e: SftpError) -> Error {
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
        other => match inner.transport_end() {
            Some(_) => inner.connection_ended(&format!("{awaiting} ({other})")),
            None => Error::new(
                ErrorKind::Io,
                format!("{awaiting}: the guest's SFTP channel failed while the connection is up ({other})"),
            ),
        },
    }
}

// Looking =============================================================================================================

impl Remote {
    /// The attributes of `path`, following symlinks; `None` when it does not exist.
    pub(super) async fn stat(&self, path: &str) -> Result<Option<FileAttributes>> {
        match self.session.metadata(path).await {
            Ok(attrs) => Ok(Some(attrs)),
            Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => Ok(None),
            Err(e) => Err(self.error(&format!("cannot inspect the guest path {path}"), e)),
        }
    }

    /// The attributes of `path` itself, a symlink not followed; `None` when it does not exist.
    pub(super) async fn lstat(&self, path: &str) -> Result<Option<FileAttributes>> {
        match self.session.symlink_metadata(path).await {
            Ok(attrs) => Ok(Some(attrs)),
            Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => Ok(None),
            Err(e) => Err(self.error(&format!("cannot inspect the guest path {path}"), e)),
        }
    }

    /// Whether the symlink `path` leads to a directory. A dangling link leads to none; a target that cannot be looked at
    /// (a loop, no permission) is an error.
    async fn leads_to_dir(&self, path: &str) -> Result<bool> {
        Ok(self.stat(path).await?.is_some_and(|attrs| attrs.file_type().is_dir()))
    }

    /// Whether the symlink `path` of a source tree leads to a directory, as far as the guest says: the link is copied
    /// as it is, so a target that cannot be looked at (a loop, no permission) counts as none.
    async fn source_leads_to_dir(&self, path: &str) -> Result<bool> {
        match self.session.metadata(path).await {
            Ok(attrs) => Ok(attrs.file_type().is_dir()),
            Err(SftpError::Status(_)) => Ok(false),
            Err(e) => Err(self.error(&format!("cannot inspect the guest path {path}"), e)),
        }
    }

    /// What the destination `path` is. A symlink is reported as leading to a directory only when `follow` is set, which
    /// looks where it leads (and fails when that cannot be told); without it, every link is `Link(Beyond::NotDir)`, for
    /// a link that the copy replaces whatever is behind it.
    pub(super) async fn present(self, path: String, follow: bool) -> Result<Present> {
        let Some(attrs) = self.lstat(&path).await? else {
            return Ok(Present::Nothing);
        };
        Ok(match attrs.file_type() {
            FileType::Dir => Present::Dir,
            FileType::Symlink if follow && self.leads_to_dir(&path).await? => Present::Link(Beyond::Dir),
            FileType::Symlink => Present::Link(Beyond::NotDir),
            FileType::File | FileType::Other => Present::File,
        })
    }

    /// The tree entry at `path`, given its attributes (not followed through a symlink).
    pub(super) async fn kind_of(&self, path: &str, attrs: &FileAttributes) -> Result<Kind> {
        // OpenSSH sends a time before 1970 as the 32-bit two's complement of its seconds, which reads as a date after
        // 2038. The two cannot be told apart, so a time past 2038 is not trusted.
        let mtime = attrs.mtime.filter(|seconds| i32::try_from(*seconds).is_ok());
        match attrs.file_type() {
            FileType::Dir => Ok(Kind::Dir { mtime }),
            FileType::File => Ok(Kind::File {
                executable: attrs.permissions.is_some_and(|mode| mode & 0o111 != 0),
                mtime,
                size: attrs.len(),
            }),
            FileType::Symlink => {
                let target = self
                    .session
                    .read_link(path)
                    .await
                    .map_err(|e| self.error(&format!("cannot read the guest symlink {path}"), e))?;
                if target.contains('\u{fffd}') {
                    return Err(Error::new(
                        ErrorKind::Io,
                        format!(
                            "the guest symlink {path} holds a target that is not valid UTF-8 or contains U+FFFD, \
                             which SFTP cannot tell apart"
                        ),
                    ));
                }
                let to_dir = self.source_leads_to_dir(path).await?;
                Ok(Kind::Link { target, to_dir })
            }
            FileType::Other => Err(Error::new(
                ErrorKind::Io,
                format!("the guest path {path} is neither a regular file, a directory nor a symlink"),
            )),
        }
    }

    /// The source `path`: a symlink is a link unless `follow` (the user wrote a trailing `/`). `None` when it does not
    /// exist.
    pub(super) async fn inspect(self, path: String, follow: bool) -> Result<Option<Kind>> {
        let attrs = if follow {
            self.stat(&path).await?
        } else {
            self.lstat(&path).await?
        };
        match attrs {
            Some(attrs) => self.kind_of(&path, &attrs).await.map(Some),
            None => Ok(None),
        }
    }

    /// The guest tree below `dir`, parents before children, symlinks as links.
    pub(super) async fn walk(self, dir: String) -> Result<Vec<Item>> {
        let mut items = Vec::new();
        let mut pending = vec![(dir, Vec::<String>::new())];
        while let Some((path, rel)) = pending.pop() {
            let awaiting = format!("cannot list the guest directory {path}");
            let listing = self
                .session
                .read_dir(path.as_str())
                .await
                .map_err(|e| self.error(&awaiting, e))?;
            let mut entries: Vec<_> = listing.map(|entry| (entry.file_name(), entry.metadata())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let names: Vec<String> = entries.iter().map(|entry| entry.0.clone()).collect();
            check_listing(&path, &names)?;
            let mut dirs = Vec::new();
            for (name, listed) in entries {
                let child = join(&path, &name);
                // A server that lists no attributes is asked.
                let attrs = if listed.permissions.is_some() {
                    listed
                } else {
                    self.lstat(&child).await?.ok_or_else(|| {
                        Error::new(
                            ErrorKind::Io,
                            format!("the guest path {child} vanished while it was listed"),
                        )
                    })?
                };
                let kind = self.kind_of(&child, &attrs).await?;
                let mut child_rel = rel.clone();
                child_rel.push(name);
                if kind.is_dir() {
                    dirs.push((child, child_rel.clone()));
                }
                items.push(Item { rel: child_rel, kind });
            }
            // Pushed in reverse so the first directory is walked first.
            pending.extend(dirs.into_iter().rev());
        }
        Ok(items)
    }
}

// Writing =============================================================================================================

impl Remote {
    /// Makes the one directory `path`, whose parent must exist, by asking for it: a server says only "failure" for a
    /// directory that is there, so a failed request is followed by a look.
    async fn mkdir(&self, path: &str) -> Result<Mkdir> {
        let Err(refused) = self.session.create_dir(path).await else {
            return Ok(Mkdir::Made);
        };
        match self.stat(path).await? {
            Some(attrs) if attrs.file_type().is_dir() => Ok(Mkdir::Existed),
            Some(_) => Err(Error::new(
                ErrorKind::Io,
                format!("the guest path {path} exists and is not a directory"),
            )),
            None => Ok(Mkdir::Refused(
                self.error(&format!("cannot create the guest directory {path}"), refused),
            )),
        }
    }

    /// Makes the directory `path`, and its missing parents first. Returns whether it made `path`. Nothing is looked at
    /// before a directory is asked for, so one made by someone else in between is found, not fought.
    pub(super) async fn create_dir(self, path: String) -> Result<bool> {
        // The directories the guest refused, `path` first, and what it said about `path`.
        let mut missing: Vec<String> = Vec::new();
        let mut first_refusal = None;
        let mut here = path;
        let made_here = loop {
            match self.mkdir(&here).await? {
                Mkdir::Made => break true,
                Mkdir::Existed => break false,
                Mkdir::Refused(error) => {
                    let parent = parent_of(&here);
                    if parent == here {
                        // A root or `.` that is missing: there is no parent left to try.
                        return Err(first_refusal.unwrap_or(error));
                    }
                    first_refusal.get_or_insert(error);
                    missing.push(here);
                    here = parent;
                }
            }
        };
        if missing.is_empty() {
            return Ok(made_here);
        }
        // The last one made is `path`.
        let mut leaf_made = false;
        for dir in missing.iter().rev() {
            leaf_made = match self.mkdir(dir).await? {
                Mkdir::Made => true,
                Mkdir::Existed => false,
                Mkdir::Refused(error) => return Err(error),
            };
        }
        Ok(leaf_made)
    }

    /// Removes the symlink `path`.
    pub(super) async fn remove_link(self, path: String) -> Result<()> {
        self.session
            .remove_file(path.as_str())
            .await
            .map_err(|e| self.error(&format!("cannot replace the guest symlink {path}"), e))
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

    /// Writes with `write` to a temporary file beside `target`, then renames it over `target`; the temporary file is
    /// gone when this fails.
    async fn replace_with<F, Fut>(&self, target: &str, write: F) -> Result<()>
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let temp = join(&parent_of(target), &partial_name());
        let written = write(temp.clone()).await;
        let moved = match written {
            Ok(()) => self.posix_rename(&temp, target).await,
            Err(e) => Err(e),
        };
        match moved {
            Ok(()) => Ok(()),
            Err(e) => {
                // A temporary that was never created is as good as removed.
                let cleanup = match self.session.remove_file(temp.as_str()).await {
                    Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => Ok(()),
                    other => other,
                };
                Err(e.after_cleanup(&format!("the guest file {temp}"), cleanup))
            }
        }
    }

    /// Streams `local` to the guest file `target`.
    pub(super) async fn put_file(
        self,
        local: PathBuf,
        target: String,
        executable: bool,
        mtime: Option<u32>,
    ) -> Result<()> {
        self.replace_with(&target, |temp| self.upload_to(&local, temp, executable, mtime))
            .await
    }

    async fn upload_to(&self, local: &Path, temp: String, executable: bool, mtime: Option<u32>) -> Result<()> {
        let awaiting = format!("cannot write the guest file {temp}");
        let attributes = FileAttributes {
            permissions: Some(if executable { 0o755 } else { 0o644 }),
            ..FileAttributes::default()
        };
        let mut file = self
            .session
            .open_with_flags_and_attributes(
                temp.as_str(),
                OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                attributes,
            )
            .await
            .map_err(|e| self.error(&awaiting, e))?;
        let source = tokio::fs::File::open(local)
            .await
            .map_err(|e| local_error("cannot read", local, e))?;
        pump(source, &mut file).await.map_err(|failed| match failed {
            Failed::Read(e) => local_error("cannot read", local, e),
            Failed::Write(e) => self.guest_io(&awaiting, e),
        })?;
        // Shutting down flushes and closes the handle: a failed write may only show here.
        file.shutdown().await.map_err(|e| self.guest_io(&awaiting, e))?;
        if let Some(time) = mtime {
            let times = FileAttributes {
                atime: Some(time),
                mtime: Some(time),
                ..FileAttributes::default()
            };
            self.session
                .set_metadata(temp.as_str(), times)
                .await
                .map_err(|e| self.error(&format!("cannot set the time of the guest file {temp}"), e))?;
        }
        Ok(())
    }

    /// Makes `target` a symlink holding `text`, in place of whatever file or symlink is there.
    pub(super) async fn put_link(self, text: String, target: String, what: String) -> Result<()> {
        let awaiting = format!("cannot copy {what} to the guest symlink {target}");
        let this = &self;
        self.replace_with(&target, |temp| async move {
            // OpenSSH's sftp-server reads the symlink request the other way round from the draft: what the link holds
            // first, where it is made second. The library names its arguments after the draft.
            this.session
                .symlink(text.as_str(), temp.as_str())
                .await
                .map_err(|e| this.error(&awaiting, e))
        })
        .await
    }

    /// Whether the guest can make symlinks where a copy to `dest` would: `None` when it can, else what it said. The
    /// probe is a link made and removed beside where the copy writes.
    pub(super) async fn probe_links(self, dest: String) -> Result<Option<String>> {
        let mut dir = dest;
        loop {
            match self.stat(&dir).await? {
                Some(attrs) if attrs.file_type().is_dir() => break,
                _ => {
                    let parent = parent_of(&dir);
                    if parent == dir {
                        return Err(Error::new(
                            ErrorKind::Io,
                            format!("the guest has no existing directory above {dir}"),
                        ));
                    }
                    dir = parent;
                }
            }
        }
        let probe = join(&dir, &partial_name());
        match self.session.symlink(".", probe.as_str()).await {
            Ok(()) => {}
            Err(SftpError::Status(status)) => {
                return Ok(Some(format!(
                    "the guest said: {}: {}",
                    status.status_code, status.error_message
                )));
            }
            Err(e) => return Err(self.error(&format!("cannot test symlinks in the guest directory {dir}"), e)),
        }
        self.session
            .remove_file(probe.as_str())
            .await
            .map_err(|e| self.error(&format!("cannot remove the test symlink {probe}"), e))?;
        Ok(None)
    }

    /// Sets the modification time of the guest directory `path`, after its children are written.
    pub(super) async fn set_dir_time(self, path: String, seconds: u32) -> Result<()> {
        let times = FileAttributes {
            atime: Some(seconds),
            mtime: Some(seconds),
            ..FileAttributes::default()
        };
        self.session
            .set_metadata(path.as_str(), times)
            .await
            .map_err(|e| self.error(&format!("cannot set the time of the guest directory {path}"), e))
    }
}

// Reading =============================================================================================================

impl Remote {
    /// Streams the guest file `remote` into the new host file `temp`.
    pub(super) async fn download(self, remote: String, temp: PathBuf) -> Result<()> {
        let awaiting = format!("cannot read the guest file {remote}");
        let file = self
            .session
            .open(remote.as_str())
            .await
            .map_err(|e| self.error(&awaiting, e))?;
        let mut local = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await
            .map_err(|e| local_error("cannot create", &temp, e))?;
        pump(file, &mut local).await.map_err(|failed| match failed {
            Failed::Read(e) => self.guest_io(&awaiting, e),
            Failed::Write(e) => local_error("cannot write", &temp, e),
        })?;
        local
            .shutdown()
            .await
            .map_err(|e| local_error("cannot finish", &temp, e))
    }

    /// The guest user's home directory.
    pub(super) async fn home(self) -> Result<String> {
        self.session
            .canonicalize(".")
            .await
            .map_err(|e| self.error("cannot find the guest home directory", e))
    }
}
