//! What an installer ISO is read for: files of its UDF tree (where their bytes lie, or their contents) and the entries
//! of its El Torito boot catalog. The ISO is only read, never mounted by the OS and never copied.

use std::fmt::Debug;
use std::path::Path;

use hadris_fs::sync::{File, FileSystem, Volume};
use hadris_fs::{Extent, MountOptions, OpenOptions, Resolve};
use hadris_iso::sync::IsoFs;
use hadris_storage::host::FileDevice;
use hadris_udf::sync::UdfFs;

use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "udf_tests.rs"]
pub(super) mod udf_tests;

/// The sector size of an ISO: El Torito counts blocks of it.
pub(super) const SECTOR: u64 = 2048;

/// A run of bytes in the ISO file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Piece {
    pub offset: u64,
    pub len: u64,
}

/// What an El Torito catalog entry says about one boot image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BootEntry {
    pub platform: Platform,
    /// The first 2048-byte block of the image the firmware loads.
    pub load_block: u32,
    pub bootable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Platform {
    Bios,
    Efi,
    Other,
}

/// An ISO file, read through its UDF tree.
pub(super) struct Udf<'a> {
    iso: &'a Path,
}

impl<'a> Udf<'a> {
    /// Checks that `iso` has a UDF tree, which every Windows installer ISO has (and which is all some of them have).
    pub(super) fn open(iso: &'a Path) -> Result<Udf<'a>> {
        let udf = Udf { iso };
        udf.mount()?;
        Ok(udf)
    }

    fn mount(&self) -> Result<UdfFs<FileDevice>> {
        let device = FileDevice::open(self.iso).map_err(|e| unreadable(self.iso, "open", &e))?;
        UdfFs::mount(device, MountOptions::new()).map_err(|e| {
            if e.kind() == hadris_fs::ErrorKind::NotRecognized {
                return Error::new(
                    ErrorKind::Recipe,
                    format!(
                        "{} is not a Windows installer ISO: it has no UDF file system",
                        self.iso.display()
                    ),
                );
            }
            unreadable(self.iso, "read the UDF file system of", &e)
        })
    }

    /// Where the bytes of the file `path` (`/`-separated, from the root) lie in the ISO, in file order; `None` when
    /// the tree has no such file. A file may lie in several pieces.
    pub(super) fn pieces(&self, path: &str) -> Result<Option<Vec<Piece>>> {
        let mut udf = self.mount()?;
        let node = match udf.resolve(path.as_bytes(), Resolve::Lexical) {
            Ok(node) => node,
            Err(e) if e.kind() == hadris_fs::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(unreadable(self.iso, &format!("look up {path} in"), &e)),
        };
        let mut found = Vec::new();
        let mut buffer = [Extent::new(0, 0); 16];
        let mut from = 0;
        loop {
            let count = udf
                .extents(node, from, &mut buffer)
                .map_err(|e| unreadable(self.iso, &format!("read where {path} is stored in"), &e))?;
            found.extend(buffer[..count].iter().map(|extent| Piece {
                offset: extent.offset(),
                len: extent.len(),
            }));
            match buffer[..count].last() {
                Some(last) if count == buffer.len() => from = last.file_offset() + last.len(),
                _ => return Ok(Some(found)),
            }
        }
    }

    /// The file `path`, readable and seekable across all its pieces; `None` when the tree has no such file.
    pub(super) fn open_file(&self, path: &str) -> Result<Option<File<UdfFs<FileDevice>>>> {
        let volume = Volume::new(self.mount()?);
        match volume.open(path, OpenOptions::new().read()) {
            Ok(file) => Ok(Some(file)),
            Err(e) if e.kind() == hadris_fs::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(unreadable(self.iso, &format!("open {path} in"), &e)),
        }
    }

    /// The entries of the El Torito boot catalog, in catalog order; none when the ISO has no boot catalog.
    pub(super) fn boot_entries(&self) -> Result<Vec<BootEntry>> {
        let device = FileDevice::open(self.iso).map_err(|e| unreadable(self.iso, "open", &e))?;
        let mut iso = IsoFs::mount(device, MountOptions::new())
            .map_err(|e| unreadable(self.iso, "read the ISO 9660 file system of", &e))?;
        let mut buffer = [0u8; SECTOR as usize];
        let catalog = iso
            .boot_catalog(&mut buffer)
            .map_err(|e| unreadable(self.iso, "read the boot catalog of", &e))?;
        Ok(catalog
            .map(|catalog| {
                catalog
                    .entries()
                    .map(|entry| BootEntry {
                        platform: match entry.platform() {
                            hadris_iso::Platform::X86 => Platform::Bios,
                            hadris_iso::Platform::Efi => Platform::Efi,
                            _ => Platform::Other,
                        },
                        load_block: entry.load_block(),
                        bootable: entry.is_bootable(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}

fn unreadable(iso: &Path, what: &str, error: &dyn Debug) -> Error {
    Error::new(ErrorKind::Io, format!("cannot {what} {}: {error:?}", iso.display()))
}
