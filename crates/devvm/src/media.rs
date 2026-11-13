//! Writing the small disk images a guest reads at first boot.

use std::io::{Cursor, Read, Write};
use std::path::Path;

use fatfs::{FatType, FileSystem, FormatVolumeOptions, FsOptions};

use crate::backend::{Media, MediaFormat, MediaKind};
use crate::host::Host;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "media_tests.rs"]
pub(crate) mod media_tests;

/// A file to put in a seed image.
#[derive(Debug)]
pub(crate) struct SeedFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

const SECTOR: u32 = 512;
/// The smallest image, in bytes. Headroom for the directory and the allocation tables is added to the contents.
const MIN_IMAGE: u64 = 1 << 20;
/// The most bytes of file contents whose image ([`image_size`]) `fatfs` 0.3.6 still formats as FAT12: at most 4,084
/// clusters of 32 KiB. Measured: one byte more fails to format ("Cannot select FAT type").
const LARGEST_FAT12_CONTENTS: u64 = 66_418_688;
const _: () = assert!(
    unchecked_image_size(LARGEST_FAT12_CONTENTS) / SECTOR as u128 <= u32::MAX as u128,
    "the sector count of the largest image must fit a FAT volume"
);
/// The volume id of every image: a fixed value keeps equal inputs byte-identical.
const VOLUME_ID: u32 = 0x4456_4d53;

/// Writes the NoCloud `cidata` image holding `files` to `into` and returns it as a guest disk of `kind`, read-only when
/// `readonly`. `spare` bytes of free room are left in the volume for the guest to write into (0 for a disk the guest
/// only reads).
///
/// The image can hold private keys, so `host` writes it as a private file ([`Host::write_private`]). An image left at
/// `into` by an earlier launch is touched only after the new image is built.
///
/// The volume is FAT12, labelled `CIDATA`. Measured: Ubuntu 24.04's cloud-init reads a FAT12 image of about 1 MiB
/// attached read-only on `virtio-blk-pci`, as the QEMU backend attaches it. Files too large for a FAT12 volume are
/// refused with `Internal` before anything is allocated.
///
/// The image is a pure function of `files`: no timestamps, a fixed volume id. The names and contents are read back from
/// the finished image before anything is written; an image that does not hold exactly `files` is an error.
pub(crate) fn write_cidata_image(
    host: &dyn Host,
    files: &[SeedFile],
    kind: MediaKind,
    readonly: bool,
    spare: u64,
    into: &Path,
) -> Result<Media> {
    refuse_path_names(files)?;
    let image = fat_image(files, spare)?;
    let written = match std::fs::remove_file(into) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
    .and_then(|()| host.write_private(into, &image));
    written.map_err(|e| {
        Error::io(
            ErrorKind::Io,
            format!("cannot write the seed image {}", into.display()),
            e,
        )
    })?;
    Ok(Media {
        path: into.to_path_buf(),
        kind,
        boot_index: None,
        readonly,
        format: MediaFormat::Raw,
    })
}

/// `fatfs` 0.3.6 trims `/` from a name and walks the rest as a path, so `a/` is `a` and `a/b` needs a directory; `.` and
/// `..` are no files. Names that merely collide or that FAT cannot store are caught by [`require_files_as_asked`].
fn refuse_path_names(files: &[SeedFile]) -> Result<()> {
    for file in files {
        if file.name.is_empty() || file.name == "." || file.name == ".." || file.name.contains(['/', '\\']) {
            return Err(unusable_name(
                &file.name,
                "it is empty, `.` or `..`, or has a path separator",
            ));
        }
    }
    Ok(())
}

fn unusable_name(name: &str, why: &str) -> Error {
    Error::new(
        ErrorKind::Internal,
        format!("`{name}` cannot be a file name in the seed image: {why}"),
    )
}

/// The messages `fatfs` 0.3.6 gives, as `ErrorKind::Other`, for a name it refuses.
const FATFS_NAME_MESSAGES: [&str; 3] = [
    "File name is empty",
    "File name too long",
    "File name contains unsupported characters",
];

/// Whether `error`, from creating a file, is about the name: an existing entry of that name, or one of
/// [`FATFS_NAME_MESSAGES`]. Anything else (a full directory, a full volume) is not the name's fault.
fn is_name_rejection(error: &std::io::Error) -> bool {
    match error.kind() {
        std::io::ErrorKind::AlreadyExists => true,
        std::io::ErrorKind::Other => FATFS_NAME_MESSAGES.contains(&error.to_string().as_str()),
        _ => false,
    }
}

fn build_failed(error: std::io::Error) -> Error {
    Error::new(ErrorKind::Internal, format!("cannot build the seed image: {error}"))
}

/// The size in bytes of the image that holds `files` bytes of files and leaves `spare` bytes free, `Internal` when that
/// is more than a FAT12 volume holds ([`LARGEST_FAT12_CONTENTS`], files and spare together).
fn image_size(files: u64, spare: u64) -> Result<u64> {
    let contents = files.saturating_add(spare);
    let size = unchecked_image_size(contents);
    if contents > LARGEST_FAT12_CONTENTS {
        let what = if spare == 0 {
            format!("the seed files ({files} bytes)")
        } else {
            format!("the seed files ({files} bytes) and {spare} bytes of spare room")
        };
        return Err(Error::new(
            ErrorKind::Internal,
            format!(
                "{what} need an image of {size} bytes, which would not be FAT12: at most {LARGEST_FAT12_CONTENTS} bytes of \
                 files and spare room fit"
            ),
        ));
    }
    // Fits `u64`: see the assertion beside `LARGEST_FAT12_CONTENTS`.
    Ok(size as u64)
}

/// The image size for `contents` bytes of files, whether or not a FAT12 volume that size holds them.
const fn unchecked_image_size(contents: u64) -> u128 {
    (MIN_IMAGE as u128 + 2 * contents as u128).next_multiple_of(SECTOR as u128)
}

/// The options that format an image of `size` bytes ([`image_size`]) as the seed volume.
///
/// `fat_type` only picks the cluster size in `fatfs` 0.3.6; the volume is FAT12 because `size` is within
/// [`LARGEST_FAT12_CONTENTS`]' bound, which [`image_size`] enforces.
fn format_options(size: u64) -> FormatVolumeOptions {
    debug_assert!(size / u64::from(SECTOR) <= u64::from(u32::MAX));
    // Fits `u32` for every size `image_size` returns (the assertion beside `LARGEST_FAT12_CONTENTS`).
    let total_sectors = (size / u64::from(SECTOR)) as u32;
    FormatVolumeOptions::new()
        .bytes_per_sector(SECTOR as u16)
        .total_sectors(total_sectors)
        .fat_type(FatType::Fat12)
        .volume_id(VOLUME_ID)
        .volume_label(*b"CIDATA     ")
}

/// The seed image, built in memory: every `fatfs` failure here is `Internal`, as nothing touches the host. `spare` bytes
/// count as contents when the image is sized, so they are free after `files` are in.
///
/// The volume is FAT12 because [`image_size`] bounds the image; `fatfs` chooses the type from the geometry, and the
/// debug assertion checks it did.
fn fat_image(files: &[SeedFile], spare: u64) -> Result<Vec<u8>> {
    let contents: u64 = files.iter().map(|file| file.bytes.len() as u64).sum();
    let size = image_size(contents, spare)?;

    let mut disk = Cursor::new(vec![0u8; size as usize]);
    fatfs::format_volume(&mut disk, format_options(size)).map_err(build_failed)?;
    {
        let fs = FileSystem::new(&mut disk, FsOptions::new()).map_err(build_failed)?;
        debug_assert_eq!(fs.fat_type(), FatType::Fat12);
        for file in files {
            let mut out = fs.root_dir().create_file(&file.name).map_err(|e| {
                if is_name_rejection(&e) {
                    unusable_name(&file.name, &e.to_string())
                } else {
                    build_failed(e)
                }
            })?;
            out.write_all(&file.bytes).map_err(build_failed)?;
            out.flush().map_err(build_failed)?;
        }
        fs.unmount().map_err(build_failed)?;
    }
    require_files_as_asked(&mut disk, files)?;
    Ok(disk.into_inner())
}

/// The size of a directory entry, and so of a slot in a directory.
const ENTRY_SIZE: u64 = 32;
/// The fewest clusters of a FAT16 volume; a FAT12 volume has fewer.
const MIN_FAT16_CLUSTERS: u64 = 4085;

/// The files of the FAT volume at `image` that are at `paths`, each named by its path as given. A path has parts
/// separated by `/` (`logs/setupact.log`), none empty, `.` or `..`; a path with such a part is refused as `Internal`.
/// A path the volume does not hold is not in the result and is no error: a guest that wrote no `logs` directory has no
/// `logs/setupact.log`. A path that names a directory, or runs through a file, is an error. A volume the guest left
/// dirty is read as it is.
///
/// `image` is a FAT12 volume devvm built (the same size as its boot sector says), and the guest's FAT driver wrote to it
/// during the base build; anything else is refused ([`Table::of`]). The allocation table is checked, because a crash can
/// tear a 12-bit slot, and nothing here walks a directory: every path has a fixed number of parts, so a directory that
/// leads back to one above it costs only the parts a path names. The clusters the volume chains are checked before
/// anything is read ([`Table::require_chains_end`]). Directory entries are trusted: a forged start cluster reads wrong
/// bytes from the in-memory copy, or panics in a debug build, and this is no defence against a hostile volume.
#[allow(dead_code, reason = "unused until the Windows base build reads back the answer disk")]
pub(crate) fn read_files(image: &Path, paths: &[&str]) -> Result<Vec<SeedFile>> {
    refuse_unreadable_paths(paths)?;
    let bytes = std::fs::read(image).map_err(|e| {
        Error::io(
            ErrorKind::Io,
            format!("cannot read the disk image {}", image.display()),
            e,
        )
    })?;
    let mut disk = Cursor::new(bytes);
    checked_volume(&mut disk)
        .and_then(|fs| read_known(&fs, paths))
        .map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot read {} as a FAT volume", image.display()),
                e,
            )
        })
}

fn refuse_unreadable_paths(paths: &[&str]) -> Result<()> {
    for path in paths {
        let unreadable = path.contains('\\')
            || path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..");
        if unreadable {
            return Err(Error::new(
                ErrorKind::Internal,
                format!(
                    "`{path}` cannot be read from an image: it is empty, or has an empty, `.` or `..` part, or a `\\`"
                ),
            ));
        }
    }
    Ok(())
}

/// The volume in `disk`, mounted once, after its allocation table is checked.
fn checked_volume(disk: &mut Cursor<Vec<u8>>) -> std::io::Result<FileSystem<&mut Cursor<Vec<u8>>>> {
    let table = Table::of(disk.get_ref())?;
    table.require_chains_end()?;
    disk.set_position(0);
    let fs = FileSystem::new(disk, FsOptions::new())?;
    if fs.fat_type() != FatType::Fat12 {
        return Err(corrupt(format!(
            "the boot sector says FAT12, the volume mounts as {:?}",
            fs.fat_type()
        )));
    }
    Ok(fs)
}

/// The files of `fs` at `paths` ([`read_files`]), those that exist.
fn read_known<T: fatfs::ReadWriteSeek>(fs: &FileSystem<T>, paths: &[&str]) -> std::io::Result<Vec<SeedFile>> {
    let named = |path: &str, e: std::io::Error| std::io::Error::new(e.kind(), format!("{path}: {e}"));
    let mut files = Vec::new();
    for path in paths {
        let mut file = match fs.root_dir().open_file(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(named(path, e)),
        };
        // The chain of the file is checked, and a read ends where the chain does, whatever size the entry claims.
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|e| named(path, e))?;
        files.push(SeedFile {
            name: (*path).to_owned(),
            bytes,
        });
    }
    Ok(files)
}

/// The names in the root directory of `fs`, files and directories.
fn root_names<T: fatfs::ReadWriteSeek>(fs: &FileSystem<T>) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs.root_dir().iter() {
        names.push(entry?.file_name());
    }
    Ok(names)
}

fn corrupt(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

/// The allocation table of a FAT12 volume, read from its boot sector.
///
/// `fatfs` follows what the table says and trusts it: a chain of clusters that leads back to itself is read for ever, a
/// value outside the volume is read from wherever it points. So the table is checked first, from the boot sector alone,
/// with the rules `fatfs` 0.3.6 applies. Only FAT12 volumes are read ([`read_files`]), and the boot sector must count
/// exactly the bytes of the image, so the cluster count, and everything sized by it, is bounded by the file itself.
struct Table<'a> {
    /// The first copy of the table, the one `fatfs` reads.
    bytes: &'a [u8],
    /// The clusters of the volume, numbered from 2.
    clusters: usize,
    /// The slots the table has, whether or not they are clusters of the volume.
    entries: usize,
}

impl<'a> Table<'a> {
    fn of(image: &'a [u8]) -> std::io::Result<Self> {
        let field = |at: usize, len: usize| -> std::io::Result<u64> {
            let bytes = image
                .get(at..at + len)
                .ok_or_else(|| corrupt("the boot sector is cut short".to_owned()))?;
            Ok(bytes.iter().rev().fold(0, |value, byte| value << 8 | u64::from(*byte)))
        };
        let (sector, per_cluster, reserved, fats) = (field(11, 2)?, field(13, 1)?, field(14, 2)?, field(16, 1)?);
        let root_entries = field(17, 2)?;
        let total = match field(19, 2)? {
            0 => field(32, 4)?,
            sectors => sectors,
        };
        // A FAT32 volume has no 16-bit sector count of the table.
        let per_fat = field(22, 2)?;
        if sector == 0 || per_cluster == 0 || per_fat == 0 {
            return Err(corrupt("the boot sector is not that of a FAT12 volume".to_owned()));
        }
        let counted = total * sector;
        if counted != image.len() as u64 {
            return Err(corrupt(format!(
                "the boot sector counts {counted} bytes, the image has {}",
                image.len()
            )));
        }
        let first_data = reserved + fats * per_fat + (root_entries * ENTRY_SIZE).div_ceil(sector);
        let clusters = total
            .checked_sub(first_data)
            .map(|sectors| sectors / per_cluster)
            .filter(|clusters| *clusters > 0)
            .ok_or_else(|| corrupt("the volume has no room for data".to_owned()))?;
        if clusters >= MIN_FAT16_CLUSTERS {
            return Err(corrupt(format!(
                "the volume has {clusters} clusters: only FAT12 volumes are read"
            )));
        }
        let start = reserved * sector;
        let bytes = usize::try_from(start)
            .ok()
            .zip(usize::try_from(per_fat * sector).ok())
            .and_then(|(start, len)| image.get(start..start.checked_add(len)?))
            .ok_or_else(|| corrupt("the allocation table lies outside the image".to_owned()))?;
        Ok(Self {
            bytes,
            // At most 4,084: the bound above.
            clusters: clusters as usize,
            entries: bytes.len() * 8 / 12,
        })
    }

    /// The cluster that follows `index` in its chain, `None` where `fatfs` ends the chain: a free slot, a bad cluster,
    /// the end mark. A value `fatfs` would follow that is not a cluster of the volume is an error.
    fn next(&self, index: usize) -> std::io::Result<Option<usize>> {
        let pair = self
            .bytes
            .get(index * 3 / 2..index * 3 / 2 + 2)
            .ok_or_else(|| corrupt(format!("cluster {index} lies beyond the allocation table")))?;
        let pair = usize::from(pair[0]) | usize::from(pair[1]) << 8;
        let value = if index.is_multiple_of(2) {
            pair & 0xFFF
        } else {
            pair >> 4
        };
        // 0xFF7 is the bad-cluster mark, 0xFF8 to 0xFFF are end marks.
        if value == 0 || value >= 0xFF7 {
            Ok(None)
        } else if (2..self.clusters + 2).contains(&value) {
            Ok(Some(value))
        } else {
            Err(corrupt(format!(
                "cluster {index} is followed by cluster {value}, outside the {} clusters of the volume",
                self.clusters
            )))
        }
    }

    /// Requires that every value in the table that `fatfs` would follow is a cluster of the volume, and that no chain of
    /// clusters leads back to one of its own. A directory entry can start a chain at any slot, so every slot is
    /// looked at; each cluster is then walked once: a cluster seen on the walk in progress is a loop, one seen on an
    /// earlier walk ends this one.
    fn require_chains_end(&self) -> std::io::Result<()> {
        // Slots 0 and 1 are reserved: they hold the media byte and the flags a driver keeps, not links.
        for index in 2..self.entries {
            self.next(index)?;
        }

        const UNSEEN: u8 = 0;
        const ON_THE_WALK: u8 = 1;
        const WALKED: u8 = 2;
        let mut state = vec![UNSEEN; self.clusters + 2];
        for start in 2..(self.clusters + 2).min(self.entries) {
            if state[start] != UNSEEN {
                continue;
            }
            let mut at = start;
            loop {
                state[at] = ON_THE_WALK;
                match self.next(at)? {
                    Some(following) if state[following] == ON_THE_WALK => {
                        return Err(corrupt(format!("cluster {following} leads back to itself")));
                    }
                    Some(following) if state[following] == UNSEEN => at = following,
                    _ => break,
                }
            }
            let mut at = start;
            while state[at] == ON_THE_WALK {
                state[at] = WALKED;
                match self.next(at)? {
                    Some(following) => at = following,
                    None => break,
                }
            }
        }
        Ok(())
    }
}

/// Reads the finished image back and requires exactly `files`: the same long names, the same count, the same contents.
///
/// `fatfs` merges names whose generated 8.3 short names, trailing dots or U+FFFF padding collide, and renames some, so
/// no list of name patterns closes the class; only the image says what it holds. The files are read by the names asked
/// for, as [`read_files`] reads them, and the root directory, one level, says what else is there.
fn require_files_as_asked(disk: &mut Cursor<Vec<u8>>, files: &[SeedFile]) -> Result<()> {
    let fs = checked_volume(disk).map_err(build_failed)?;
    let mut held_names = root_names(&fs).map_err(build_failed)?;
    let mut asked_names: Vec<&str> = files.iter().map(|file| file.name.as_str()).collect();
    held_names.sort_unstable();
    asked_names.sort_unstable();
    if held_names != asked_names {
        return Err(Error::new(
            ErrorKind::Internal,
            format!(
                "the seed image would hold {held_names:?} where {asked_names:?} were asked: FAT keeps one entry for \
                 names it treats as equal and rewrites names it cannot store as they are"
            ),
        ));
    }
    let held = read_known(&fs, &asked_names).map_err(build_failed)?;
    for file in files {
        match held.iter().find(|held| held.name == file.name) {
            Some(held) if held.bytes == file.bytes => {}
            Some(_) => return Err(unusable_name(&file.name, "the image holds other bytes under that name")),
            None => return Err(unusable_name(&file.name, "the image holds no file of that name")),
        }
    }
    Ok(())
}
