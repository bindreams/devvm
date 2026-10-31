//! The guest side of a copy, as [`super::copy`] drives it: each step is asked of the guest's SFTP server through
//! [`super::remote::Remote`] on the runtime and waited for here.

use std::path::{Path, PathBuf};

use super::Sftp;
use super::copy::{CopySide, Receives, slash_on_non_dir};
use super::local::HostSide;
use super::plan::{self, Item, Kind, Present};
use crate::guest::GuestOs;
use crate::{Error, ErrorKind, Result};

/// The guest end of a copy.
pub(super) struct GuestSide<'a> {
    pub(super) sftp: &'a Sftp,
    pub(super) guest: &'a dyn GuestOs,
    /// Whether the source tree is told what each of its links leads to: only a host that makes the two kinds of link
    /// differently uses it, and asking for it can fail where the copy would not.
    pub(super) link_kinds: bool,
}

impl GuestSide<'_> {
    /// Streams the guest file `from` into the new host file `temp`, which becomes `to`.
    pub(super) fn download(&self, from: &str, temp: &Path, to: &str) -> Result<()> {
        let (from, temp) = (from.to_owned(), temp.to_owned());
        self.sftp.call(
            &format!("copying the guest file {} to {}", self.sftp.remote.shown(&from), to),
            move |remote| remote.download(from, temp),
        )
    }
}

impl CopySide for GuestSide<'_> {
    type Path = String;

    const NAME: &'static str = "guest";
    const AS_DESTINATION: &'static str = "the guest";
    const SOURCE_TIMES: &'static str = "1970 to 2038";

    fn show(&self, path: &String) -> String {
        self.sftp.remote.shown(path)
    }

    fn join(&self, base: &String, name: &str) -> String {
        plan::join(base, name)
    }

    fn parent(&self, path: &String) -> Option<String> {
        Some(plan::parent_of(path))
    }

    fn file_name(&self, path: &String) -> Result<String> {
        plan::base_name(path).map(str::to_owned).ok_or_else(|| {
            Error::new(
                ErrorKind::Usage,
                format!("the guest path {} has no file name to copy", self.show(path)),
            )
        })
    }

    fn chain(&self, path: &String) -> Vec<(Option<String>, String)> {
        plan::chain(path)
    }

    fn file_name_problem(&self, name: &str) -> Option<String> {
        self.guest.file_name_problem(name)
    }

    fn inspect(&self, path: &String, follow: bool) -> Result<Kind> {
        let inspect = |path: &String| {
            self.sftp
                .call(&format!("inspecting the guest path {}", self.show(path)), {
                    let path = path.clone();
                    let link_kinds = self.link_kinds;
                    move |remote| remote.inspect(path, follow, link_kinds)
                })
        };
        if let Some(kind) = inspect(path)? {
            return Ok(kind);
        }
        // A server answers `file/` as it does a missing path, though it is there: the `/` is what it cannot take.
        let bare = path.trim_end_matches('/').to_owned();
        if follow && !bare.is_empty() && bare != *path && inspect(&bare)?.is_some_and(|kind| !kind.is_dir()) {
            return Err(slash_on_non_dir(Self::NAME, &self.show(path)));
        }
        Err(Error::new(
            ErrorKind::NotFound,
            format!("the guest source {} does not exist", self.show(path)),
        ))
    }

    fn walk(&self, dir: &String) -> Result<Vec<Item>> {
        self.sftp
            .call(&format!("listing the guest directory {}", self.show(dir)), {
                let dir = dir.clone();
                let link_kinds = self.link_kinds;
                move |remote| remote.walk(dir, link_kinds)
            })
    }

    fn present(&self, path: &String, follow: bool) -> Result<Present> {
        self.sftp
            .call(&format!("inspecting the guest path {}", self.show(path)), {
                let path = path.clone();
                move |remote| remote.present(path, follow)
            })
    }

    fn create_dir(&self, dir: &String) -> Result<bool> {
        self.sftp
            .call(&format!("creating the guest directory {}", self.show(dir)), {
                let dir = dir.clone();
                move |remote| remote.create_dir(dir)
            })
    }

    fn remove_link(&self, path: &String) -> Result<()> {
        self.sftp
            .call(&format!("replacing the guest symlink {}", self.show(path)), {
                let path = path.clone();
                move |remote| remote.remove_link(path)
            })
    }

    fn put_link(&self, text: &str, _to_dir: bool, path: &String, what: &str) -> Result<()> {
        self.sftp
            .call(&format!("copying {what} to the guest path {}", self.show(path)), {
                let (text, path, what) = (text.to_owned(), path.clone(), what.to_owned());
                move |remote| remote.put_link(text, path, what)
            })
    }

    fn set_dir_time(&self, path: &String, seconds: u32) -> Result<()> {
        self.sftp.call(
            &format!("setting the time of the guest directory {}", self.show(path)),
            {
                let path = path.clone();
                move |remote| remote.set_dir_time(path, seconds)
            },
        )
    }

    fn probe_links(&self, dest: &String) -> Result<Option<String>> {
        self.sftp
            .call(&format!("testing symlinks in the guest path {}", self.show(dest)), {
                let dest = dest.clone();
                move |remote| remote.probe_links(dest)
            })
    }
}

impl Receives<HostSide<'_>> for GuestSide<'_> {
    fn put_file(
        &self,
        source: &HostSide<'_>,
        from: &PathBuf,
        executable: bool,
        mtime: Option<u32>,
        to: &String,
    ) -> Result<()> {
        self.sftp.call(
            &format!("copying {} to the guest path {}", source.show(from), self.show(to)),
            {
                let (from, to) = (from.clone(), to.clone());
                move |remote| remote.put_file(from, to, executable, mtime)
            },
        )
    }

    fn announce(&self, from: &str, to: &str, total: &str) -> String {
        format!("copying {from} to the guest ({to}): {total}")
    }
}
