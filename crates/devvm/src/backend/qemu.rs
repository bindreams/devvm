//! The QEMU backend: a pinned sidecar build, driven over QMP.

pub(crate) mod args;
pub(crate) mod backend;
pub(crate) mod machine;
pub(crate) mod ports;
pub(crate) mod qmp;
pub(crate) mod sidecar;
pub(crate) mod storage;
pub(crate) mod uds;

use std::path::Path;

use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "qemu_tests.rs"]
mod qemu_tests;

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
