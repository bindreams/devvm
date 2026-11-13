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
                "{what} need an image of {size} bytes, which would not be FAT12:                  at most {LARGEST_FAT12_CONTENTS} bytes of files and spare room fit"
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

/// Every file of the FAT volume at `image`, in every directory, named by its path with `/` between the parts (a file a
/// guest wrote to `logs\setupact.log` is `logs/setupact.log`). A directory with no files adds nothing, so a guest
/// that wrote no `logs` directory has no such files and no error. A volume the guest left dirty is read as it is.
#[allow(dead_code, reason = "unused until the Windows base build reads back the answer disk")]
pub(crate) fn read_files(image: &Path) -> Result<Vec<SeedFile>> {
    let bytes = std::fs::read(image).map_err(|e| {
        Error::io(
            ErrorKind::Io,
            format!("cannot read the disk image {}", image.display()),
            e,
        )
    })?;
    tree_of(&mut Cursor::new(bytes)).map_err(|e| {
        Error::io(
            ErrorKind::Io,
            format!("cannot read {} as a FAT volume", image.display()),
            e,
        )
    })
}

/// The files of the volume in `disk`, as [`read_files`].
///
/// A guest wrote the volume, so it is not trusted to be a tree. `fatfs` follows what it finds: a chain of clusters that
/// leads back to itself is read for ever. The reading refuses every loop and counts nothing arbitrary: the clusters
/// of a chain are checked once each ([`require_chains_end`]), and an honest volume holds at most one 32-byte entry per
/// 32 bytes of the image and one byte of content per byte of the image, which bounds the walk through directories that
/// lead back to a directory above them.
fn tree_of(disk: &mut Cursor<Vec<u8>>) -> std::io::Result<Vec<SeedFile>> {
    disk.set_position(0);
    let (fat_type, clusters) = {
        let fs = FileSystem::new(&mut *disk, FsOptions::new())?;
        (fs.fat_type(), fs.stats()?.total_clusters())
    };
    let image_len = disk.get_ref().len() as u64;
    require_chains_end(disk.get_ref(), fat_type, clusters)?;

    disk.set_position(0);
    let fs = FileSystem::new(disk, FsOptions::new())?;
    let mut entries_left = image_len / ENTRY_SIZE;
    let mut bytes_left = image_len;
    let mut files = Vec::new();
    let mut directories = vec![(fs.root_dir(), String::new())];
    while let Some((dir, prefix)) = directories.pop() {
        for entry in dir.iter() {
            let entry = entry?;
            entries_left = entries_left
                .checked_sub(1)
                .ok_or_else(|| corrupt("the directories lead back to a directory above them".to_owned()))?;
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let path = format!("{prefix}{name}");
            if entry.is_dir() {
                directories.push((entry.to_dir(), format!("{path}/")));
            } else {
                // One byte past what is left shows that the file is more than the volume has room for.
                let mut bytes = Vec::new();
                entry.to_file().take(bytes_left + 1).read_to_end(&mut bytes)?;
                bytes_left = bytes_left
                    .checked_sub(bytes.len() as u64)
                    .ok_or_else(|| corrupt("the files hold more bytes than the volume has room for".to_owned()))?;
                files.push(SeedFile { name: path, bytes });
            }
        }
    }
    Ok(files)
}

/// The size of a directory entry, and so of a slot in a directory.
const ENTRY_SIZE: u64 = 32;

fn corrupt(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

/// Requires that no chain of clusters in the allocation table of the FAT volume `image` leads back to one of its own
/// clusters. Every cluster is walked once: a cluster seen on the walk in progress is a loop, one seen on an earlier walk
/// ends this one.
fn require_chains_end(image: &[u8], fat_type: FatType, clusters: u32) -> std::io::Result<()> {
    let word = |at: usize| {
        image
            .get(at..at + 2)
            .map(|b| u32::from(u16::from_le_bytes([b[0], b[1]])))
    };
    let sector = word(11).unwrap_or(0) as usize;
    let table = word(14).unwrap_or(0) as usize * sector;
    let next = |cluster: usize| -> Option<usize> {
        let value = match fat_type {
            FatType::Fat12 => {
                let at = table + cluster * 3 / 2;
                let pair = word(at)?;
                if cluster.is_multiple_of(2) {
                    pair & 0xFFF
                } else {
                    pair >> 4
                }
            }
            FatType::Fat16 => word(table + cluster * 2)?,
            FatType::Fat32 => {
                let at = table + cluster * 4;
                (word(at)? | word(at + 2)? << 16) & 0x0FFF_FFFF
            }
        } as usize;
        (2..clusters as usize + 2).contains(&value).then_some(value)
    };

    const UNSEEN: u8 = 0;
    const ON_THE_WALK: u8 = 1;
    const WALKED: u8 = 2;
    let mut state = vec![UNSEEN; clusters as usize + 2];
    for start in 2..clusters as usize + 2 {
        if state[start] != UNSEEN {
            continue;
        }
        let mut at = start;
        loop {
            state[at] = ON_THE_WALK;
            match next(at) {
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
            match next(at) {
                Some(following) => at = following,
                None => break,
            }
        }
    }
    Ok(())
}

/// Reads the finished image back with [`read_files`]' reader and requires exactly `files`: the same long names, the same
/// count, the same contents.
///
/// `fatfs` merges names whose generated 8.3 short names, trailing dots or U+FFFF padding collide, and renames some, so
/// no list of name patterns closes the class; only the image says what it holds.
fn require_files_as_asked(disk: &mut Cursor<Vec<u8>>, files: &[SeedFile]) -> Result<()> {
    let held = tree_of(disk).map_err(build_failed)?;
    let held: Vec<(String, Vec<u8>)> = held.into_iter().map(|file| (file.name, file.bytes)).collect();

    let mut held_names: Vec<&str> = held.iter().map(|(name, _)| name.as_str()).collect();
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
    for file in files {
        if let Some((_, bytes)) = held.iter().find(|(name, _)| *name == file.name)
            && *bytes != file.bytes
        {
            return Err(unusable_name(&file.name, "the image holds other bytes under that name"));
        }
    }
    Ok(())
}
