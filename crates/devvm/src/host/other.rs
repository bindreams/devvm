//! A host with no mode bits to set and no symlinks to make. It is the host on no supported platform; it exists so that
//! the tests of what every unsupported host shares run everywhere.

use std::fs::OpenOptions;
use std::path::Path;

use super::unsupported::FileModes;

pub(super) struct OtherHost;

impl FileModes for OtherHost {
    fn private_options(&self) -> OpenOptions {
        OpenOptions::new()
    }

    fn is_executable(&self, _path: &Path) -> std::io::Result<bool> {
        Ok(false)
    }

    fn set_executable(&self, _path: &Path) -> std::io::Result<()> {
        Ok(())
    }

    fn read_link(&self, _path: &Path) -> std::io::Result<String> {
        Err(std::io::Error::other("this build has no symlink support"))
    }

    fn make_symlink(&self, _target: &str, _link: &Path, _to_dir: bool) -> std::io::Result<()> {
        Err(std::io::Error::other("this build has no symlink support"))
    }
}
