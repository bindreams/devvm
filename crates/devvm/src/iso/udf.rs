//! What an installer ISO is read for: files of its UDF tree (where their bytes lie, or their contents) and the entries
//! of its El Torito boot catalog. The ISO is only read, never mounted by the OS and never copied.

use std::fmt::Debug;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use hadris_fs::sync::{File, FileSystem, Volume};
use hadris_fs::{Extent, FileType, MountOptions, OpenOptions, Resolve};
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

/// How many extents `Udf::pieces` asks for at a time.
const PAGE: usize = 16;

/// Why a file's extents are not pieces.
#[derive(Debug)]
enum MapError<E> {
    /// Asking for extents failed.
    Page(E),
    /// The extents do not map the file's bytes; what is wrong, to follow the file's name.
    Refused(&'static str),
}

/// The extents of a file of `length` bytes, from `page(from, out)` (which fills `out` with the extents that end after
/// byte `from`, in file order, and returns how many), as pieces. The extents must follow one another from byte 0 to
/// `length`: a hole (which `extents` leaves out) anywhere makes a gap, and the bytes of an unwritten part are not in
/// the ISO.
fn map_extents<E>(
    length: u64,
    mut page: impl FnMut(u64, &mut [Extent]) -> std::result::Result<usize, E>,
) -> std::result::Result<Vec<Piece>, MapError<E>> {
    let mut found = Vec::new();
    let mut buffer = [Extent::new(0, 0); PAGE];
    let mut from = 0;
    // Where the extents so far end in the file.
    let mut end = 0u64;
    loop {
        let count = page(from, &mut buffer).map_err(MapError::Page)?;
        for extent in &buffer[..count] {
            if extent.is_unwritten() {
                return Err(MapError::Refused("has a part that was never written"));
            }
            match extent.file_offset() {
                start if start > end => return Err(MapError::Refused("has no storage (a gap)")),
                start if start < end => return Err(MapError::Refused("has parts that overlap")),
                _ => {}
            }
            end = end
                .checked_add(extent.len())
                .ok_or(MapError::Refused("has a part whose end is out of range"))?;
            if end > length {
                return Err(MapError::Refused("has a part past its end"));
            }
            found.push(Piece {
                offset: extent.offset(),
                len: extent.len(),
            });
        }
        if count < buffer.len() {
            break;
        }
        // A full page: the next one starts where this one ended, which must be further on.
        if end <= from {
            return Err(MapError::Refused("has a list of parts that does not advance"));
        }
        from = end;
    }
    if end != length {
        return Err(MapError::Refused("has no storage (a gap)"));
    }
    Ok(found)
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

/// An ISO file, read through its UDF tree. The file is opened by the caller and everything is read through that one
/// handle.
pub(super) struct Udf<'a> {
    iso: &'a Path,
    file: std::fs::File,
    volume: Volume<UdfFs<FileDevice>>,
}

impl<'a> Udf<'a> {
    /// Mounts the UDF tree of `file`, the open `iso`, which every Windows installer ISO has (and which is all some of
    /// them have).
    pub(super) fn open(iso: &'a Path, file: std::fs::File) -> Result<Udf<'a>> {
        let device = FileDevice::new(file.try_clone().map_err(|e| unreadable(iso, "open", &e))?)
            .map_err(|e| unreadable(iso, "open", &e))?;
        let fs = UdfFs::mount(device, MountOptions::new()).map_err(|e| {
            if e.kind() == hadris_fs::ErrorKind::NotRecognized {
                return Error::new(
                    ErrorKind::Recipe,
                    format!(
                        "{} is not a Windows installer ISO: it has no UDF file system",
                        iso.display()
                    ),
                );
            }
            unreadable(iso, "read the UDF file system of", &e)
        })?;
        Ok(Udf {
            iso,
            file,
            volume: Volume::new(fs),
        })
    }

    /// `path` (`/`-separated, from the root) spelled as the tree spells it: UDF names are exact-case, and the case of
    /// the names an installer ISO needs differs between ISOs. `None` when the tree has no such file; an error when two
    /// names in it match a component and differ only in case.
    fn spelled(&self, path: &str) -> Result<Option<String>> {
        let mut spelled = String::new();
        for component in path.split('/').filter(|component| !component.is_empty()) {
            let look_up = || format!("look up {path} in");
            let dir = self
                .volume
                .read_dir(if spelled.is_empty() { "/" } else { &spelled })
                .map(Some)
                .or_else(|e| match e.kind() {
                    hadris_fs::ErrorKind::NotFound | hadris_fs::ErrorKind::NotADirectory => Ok(None),
                    _ => Err(unreadable(self.iso, &look_up(), &e)),
                })?;
            let Some(dir) = dir else { return Ok(None) };
            let mut matching = Vec::new();
            for entry in dir {
                let entry = entry.map_err(|e| unreadable(self.iso, &look_up(), &e))?;
                if entry.name().as_bytes().eq_ignore_ascii_case(component.as_bytes()) {
                    matching.push(String::from_utf8_lossy(entry.name().as_bytes()).into_owned());
                }
            }
            match &matching[..] {
                [] => return Ok(None),
                [name] => {
                    spelled.push('/');
                    spelled.push_str(name);
                }
                several => {
                    return Err(Error::new(
                        ErrorKind::Recipe,
                        format!(
                            "{} has {} in the same directory, which differ only in case, so devvm cannot tell which is {}",
                            self.iso.display(),
                            several
                                .iter()
                                .map(|name| format!("`{name}`"))
                                .collect::<Vec<_>>()
                                .join(" and "),
                            path.trim_start_matches('/')
                        ),
                    ));
                }
            }
        }
        Ok(Some(spelled))
    }

    /// Where the bytes of the file `path` lie in the ISO, in file order; `None` when the tree has no such file. A file
    /// may lie in several pieces. Refuses anything but a regular file whose parts are contiguous, all hold data and
    /// cover its length (a directory, a link, an unwritten extent, a hole anywhere): its bytes would not be in the ISO
    /// where the pieces say. For mapping raw bytes, as the boot images need; `open_file` reads a file whole.
    pub(super) fn pieces(&self, path: &str) -> Result<Option<Vec<Piece>>> {
        let Some(path) = self.spelled(path)? else {
            return Ok(None);
        };
        let mut udf = self
            .volume
            .lock()
            .map_err(|e| unreadable(self.iso, &format!("look up {path} in"), &e))?;
        let node = match udf.resolve(path.as_bytes(), Resolve::Lexical) {
            Ok(node) => node,
            Err(e) if e.kind() == hadris_fs::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(unreadable(self.iso, &format!("look up {path} in"), &e)),
        };
        let refused = |why: &str| {
            Error::new(
                ErrorKind::Recipe,
                format!(
                    "{} is not a Windows installer ISO: {} {why}",
                    self.iso.display(),
                    path.trim_start_matches('/')
                ),
            )
        };
        let meta = udf
            .stat(node)
            .map_err(|e| unreadable(self.iso, &format!("read the size of {path} in"), &e))?;
        match meta.file_type() {
            FileType::File => {}
            FileType::Dir => return Err(refused("is a directory, not a file")),
            FileType::Symlink => return Err(refused("is a link, not a file")),
            _ => return Err(refused("is not a file")),
        }
        map_extents(meta.len(), |from, out| udf.extents(node, from, out))
            .map(Some)
            .map_err(|e| match e {
                MapError::Page(e) => unreadable(self.iso, &format!("read where {path} is stored in"), &e),
                MapError::Refused(why) => refused(why),
            })
    }

    /// The file `path`, readable and seekable across all its pieces; `None` when the tree has no such file.
    pub(super) fn open_file(&self, path: &str) -> Result<Option<File<UdfFs<FileDevice>>>> {
        let Some(path) = self.spelled(path)? else {
            return Ok(None);
        };
        match self.volume.open(&path, OpenOptions::new().read()) {
            Ok(file) => Ok(Some(file)),
            Err(e) if e.kind() == hadris_fs::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(unreadable(self.iso, &format!("open {path} in"), &e)),
        }
    }

    /// The bytes of `pieces`, one after the other.
    pub(super) fn read_pieces(&self, pieces: &[Piece]) -> Result<Vec<u8>> {
        let fail = |e| unreadable(self.iso, "read", &e);
        let length = self.file.metadata().map_err(fail)?.len();
        let mut file = &self.file;
        let mut bytes = Vec::new();
        for piece in pieces {
            if piece.offset.checked_add(piece.len).is_none_or(|end| end > length) {
                return Err(Error::new(
                    ErrorKind::Recipe,
                    format!(
                        "{} is not a Windows installer ISO: {} bytes at {} lie outside the file",
                        self.iso.display(),
                        piece.len,
                        piece.offset
                    ),
                ));
            }
            file.seek(SeekFrom::Start(piece.offset)).map_err(fail)?;
            let start = bytes.len();
            bytes.resize(start + piece.len as usize, 0);
            file.read_exact(&mut bytes[start..]).map_err(fail)?;
        }
        Ok(bytes)
    }

    /// The entries of the El Torito boot catalog, in catalog order; none when the ISO has no boot catalog.
    pub(super) fn boot_entries(&self) -> Result<Vec<BootEntry>> {
        let device = FileDevice::new(self.file.try_clone().map_err(|e| unreadable(self.iso, "open", &e))?)
            .map_err(|e| unreadable(self.iso, "open", &e))?;
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
