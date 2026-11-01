//! A host with no mode bits to set and no symlinks to make. It is the host on no supported platform; it exists so that
//! the tests of what every unsupported host shares run everywhere.

use std::fs::OpenOptions;
use std::path::Path;

use super::Entry;
use super::unsupported::{FileModes, claim_then_rename};

pub(super) struct OtherHost;

impl FileModes for OtherHost {
    fn private_options(&self) -> OpenOptions {
        OpenOptions::new()
    }

    /// `sun_path` is 108 bytes, as on Windows, the platform whose limit this host copies.
    fn socket_path_max(&self) -> usize {
        107
    }

    fn is_executable(&self, _path: &Path) -> std::io::Result<bool> {
        Ok(false)
    }

    fn set_executable(&self, _path: &Path) -> std::io::Result<()> {
        Ok(())
    }

    fn sync_dir(&self, _dir: &Path) -> std::io::Result<()> {
        Ok(())
    }

    /// A kill between the two steps leaves `to` empty; this host exists for tests only.
    fn publish_dir(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        claim_then_rename(from, to)
    }

    fn read_link(&self, _path: &Path) -> std::io::Result<String> {
        Err(std::io::Error::other("this build has no symlink support"))
    }

    fn make_symlink(&self, _target: &str, _link: &Path, _to_dir: bool) -> std::io::Result<()> {
        Err(std::io::Error::other("this build has no symlink support"))
    }

    fn open_entry(&self, _root: &Path, _rel: &str) -> std::io::Result<Entry> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this build cannot look at a path by name",
        ))
    }
}
