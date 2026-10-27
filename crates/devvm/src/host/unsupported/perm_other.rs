//! File permissions where there are no mode bits to set: only the Windows build of this module, used by tests.

use std::fs::OpenOptions;
use std::path::Path;

pub(super) fn private_options() -> OpenOptions {
    OpenOptions::new()
}

pub(super) fn is_executable(_path: &Path) -> std::io::Result<bool> {
    Ok(false)
}

pub(super) fn set_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
