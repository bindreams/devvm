//! The QEMU backend: a pinned sidecar build, driven over QMP.

pub(crate) mod args;
pub(crate) mod backend;
pub(crate) mod machine;
pub(crate) mod ports;
pub(crate) mod qmp;
pub(crate) mod sidecar;
pub(crate) mod storage;
mod sweep;
pub(crate) mod uds;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use self::backend::QemuBackend;
use self::sidecar::LazyInstall;
use self::storage::QemuStorage;
use crate::config::Config;
use crate::exit::Progress;
use crate::guest::linux::{LinuxBuilder, LinuxGuests};
use crate::host::Host;
use crate::store::fetch::Fetcher;
use crate::{Dirs, Error, ErrorKind, Result};

#[cfg(test)]
#[path = "qemu_tests.rs"]
mod qemu_tests;

/// The QEMU backend and the guests that go with it: what an engine for this build is made of.
pub(crate) struct QemuParts {
    pub backend: Arc<QemuBackend>,
    pub guests: Arc<LinuxGuests>,
}

/// The QEMU backend and the Linux guest over one `LazyInstall` and one `QemuStorage`, which the guest's builder reaches
/// through `Storage::import_base`. Nothing is resolved, downloaded or created here: only a launch, a disk, a layer
/// commit or a base import resolve the install, so `ls`, `rm` and `stop` work offline. On a host with no manifest entry
/// the parts still build, and the first use that needs QEMU fails naming the triple.
pub(crate) fn qemu_parts(
    dirs: &Dirs,
    run: Result<PathBuf>,
    config: Config,
    fetcher: Arc<Fetcher>,
    host: &'static dyn Host,
    progress: Arc<dyn Progress>,
) -> QemuParts {
    let install = Arc::new(LazyInstall::new(
        dirs.clone(),
        config,
        fetcher.clone(),
        progress.clone(),
        host,
    ));
    let storage = Arc::new(QemuStorage::new(install.clone(), host));
    let builder = Arc::new(LinuxBuilder::new(fetcher, storage.clone()));
    QemuParts {
        backend: Arc::new(QemuBackend::new(install, storage, host, run, progress)),
        guests: Arc::new(LinuxGuests::new(builder)),
    }
}

/// The directory that holds `path`, which is `what`. Every path devvm gives the backend is absolute and in a directory, so
/// a path without one is a bug in the caller, named here instead of resolved against the working directory.
fn directory_of<'a>(path: &'a Path, what: &str) -> Result<&'a Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Internal,
                format!("{what} {} has no directory to put its files in", path.display()),
            )
        })
}
