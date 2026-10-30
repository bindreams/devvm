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

pub(super) fn read_link(_path: &Path) -> std::io::Result<String> {
    Err(std::io::Error::other("this build has no symlink support"))
}

pub(super) fn make_symlink(_target: &str, _link: &Path, _to_dir: bool) -> std::io::Result<()> {
    Err(std::io::Error::other("this build has no symlink support"))
}
