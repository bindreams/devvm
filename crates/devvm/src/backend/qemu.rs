//! The QEMU backend: a pinned sidecar build, driven over QMP.

pub(crate) mod args;
pub(crate) mod backend;
mod checks;
pub(crate) mod machine;
pub(crate) mod ports;
pub(crate) mod qmp;
pub(crate) mod sidecar;
pub(crate) mod storage;
mod sweep;
pub(crate) mod uds;

use std::path::Path;
use std::sync::Arc;

use self::backend::QemuBackend;
use self::sidecar::LazyInstall;
use self::storage::QemuStorage;
use crate::Dirs;
use crate::exit::Progress;
use crate::guest::linux::{LinuxBuilder, QemuGuests};
use crate::host::Host;
use crate::store::fetch::Fetcher;

#[cfg(test)]
#[path = "qemu_tests.rs"]
mod qemu_tests;

/// The QEMU backend and the guests that go with it.
pub(crate) struct QemuParts {
    pub backend: Arc<QemuBackend>,
    pub guests: Arc<QemuGuests>,
    /// The install both of them resolve from.
    pub install: Arc<LazyInstall>,
}

/// The QEMU backend and the Linux guest over one `LazyInstall` and one `QemuStorage`, which the guest's builder reaches
/// through `Storage::import_base`. Nothing is resolved, downloaded or created here: only a launch, a disk, a layer
/// commit or a base import resolve the install, so `ls`, `rm` and `stop` work offline. Only those uses,
/// `Source::existing` and the engine's check that the file loads read `config.yaml`, through `install.config()`. On a
/// host with no manifest entry the parts still build, and the first use that needs QEMU fails naming the triple.
pub(crate) fn qemu_parts(
    dirs: &Dirs,
    fetcher: Arc<Fetcher>,
    host: &'static dyn Host,
    progress: Arc<dyn Progress>,
) -> QemuParts {
    let install = Arc::new(LazyInstall::new(dirs.clone(), fetcher.clone(), progress.clone(), host));
    let storage = Arc::new(QemuStorage::new(install.clone(), host));
    let builder = Arc::new(LinuxBuilder::new(fetcher, storage.clone()));
    QemuParts {
        backend: Arc::new(QemuBackend::new(
            install.clone(),
            storage,
            host,
            dirs.run().map(Path::to_path_buf),
            progress,
        )),
        guests: Arc::new(QemuGuests::new(builder)),
        install,
    }
}
