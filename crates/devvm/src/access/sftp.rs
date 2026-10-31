//! SFTP file copy between the host and a guest.
//!
//! One source goes to one destination, and where it lands depends on the source and the destination only (the rules are
//! [`plan::place`]):
//! - A directory *becomes* the destination: its contents merge into it, it is created with its parents when missing, and
//!   it never nests, so a rerun gives the same layout.
//! - A file goes inside a destination that is a directory or ends in `/`, and is the destination otherwise.
//! - Symlinks are copied as links, never followed; a source path that is itself a link is copied as a link unless it
//!   ends in `/`. A destination that is a link to a directory is followed, with or without `/`; one that is a link to a
//!   file or to nothing counts as a file, and one whose target cannot be looked at (a loop, no permission) is an error.
//!   Links met inside the merged tree are replaced. A host or guest that cannot store a link is an error naming it.
//! - `.` and `..` in the destination are read by name before anything is made (a symlink before a `..` is not
//!   followed), so `new/.` is `new` and `a/../b` is `b`, with no stray `a`.
//! - Contents, the execute bit and the modification time of files are kept (through [`Host`] where the host decides).
//! - Every file and link is written to `.devvm-partial-<random>` beside its target and renamed over it (on the guest
//!   side with `posix-rename@openssh.com`), so a failure leaves no partial file at the final path, a read-only target
//!   is replaced, and a symlink at the destination is replaced, never written through.
//! - Directories the copy creates keep their modification time too, set after their contents are written; one that was
//!   already there keeps its own, since setting a time needs ownership. A time SFTP cannot carry for a file or a
//!   directory the copy creates (before 1970, after 2106, or none shown; from a guest, after 2038 as well) is named in
//!   a line and the copy goes on.
//! - The whole source is walked before anything is written: a name the target cannot store (the destination's own
//!   included), an entry that would land on something of the wrong kind, and a link the target cannot make fail the
//!   copy, naming it. Nothing at the destination is deleted.
//!
//! One algorithm ([`copy`]) serves both directions over the two ends of a copy: the host ([`local`]), which runs on the
//! calling thread, and the guest ([`guest`]), whose steps ([`remote`]) run on the runtime, bounded by
//! [`Inner::guarded`].

mod copy;
mod guest;
mod local;
mod plan;
mod remote;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use self::guest::GuestSide;
use self::local::HostSide;
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

    /// The guest path `path` with `~` expanded, and the guest's home.
    fn expanded(&self, path: &str) -> Result<(String, String)> {
        check_guest_path(path)?;
        let home = self.home()?;
        Ok((expand_guest_path(path, &home), home))
    }

    /// The guest path as the guest's SFTP server expects it.
    fn guest_path(&self, path: &str, guest: &dyn GuestOs) -> Result<String> {
        let (expanded, home) = self.expanded(path)?;
        Ok(guest.sftp_path(&expanded, &home))
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
        let (expanded, home) = self.expanded(dest)?;
        let dest = without_slash(&guest.sftp_path(&plan::normalize(&expanded), &home));
        let follow = local::ends_with_separator(src);
        copy::copy(
            &HostSide { host },
            &src.to_owned(),
            follow,
            &GuestSide { sftp: self, guest },
            &dest,
            slash,
            progress,
        )
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
        let dest = local::lexically_normal(&local::without_trailing_separator(dest));
        let follow = src.ends_with('/');
        let src = self.guest_path(src, guest)?;
        copy::copy(
            &GuestSide { sftp: self, guest },
            &src,
            follow,
            &HostSide { host },
            &dest,
            slash,
            progress,
        )
    }
}
