//! Writing the small disk images a guest reads at first boot.

use std::io::{Cursor, Read, Write};
use std::path::Path;

use fatfs::{FatType, FileSystem, FormatVolumeOptions, FsOptions};

use crate::backend::{Media, MediaKind};
use crate::host::Host;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "media_tests.rs"]
pub(crate) mod media_tests;

/// A file to put in a seed image.
pub(crate) struct SeedFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

const SECTOR: u32 = 512;
/// The smallest image, in bytes. Headroom for the directory and the allocation tables is added to the contents.
const MIN_IMAGE: u64 = 1 << 20;
/// The most bytes of file contents whose image ([`image_size`]) `fatfs` 0.3.6 still formats as FAT12: at most 4,084
/// clusters of 32 KiB. Measured; one byte more makes FAT16.
const LARGEST_FAT12_CONTENTS: u64 = 66_418_688;
const _: () = assert!(
    (MIN_IMAGE + 2 * LARGEST_FAT12_CONTENTS).div_ceil(SECTOR as u64) <= u32::MAX as u64,
    "the sector count of the largest image must fit a FAT volume"
);
/// The volume id of every image: a fixed value keeps equal inputs byte-identical.
const VOLUME_ID: u32 = 0x4456_4d53;

/// Writes the NoCloud `cidata` image holding `files` to `into` and returns it as a read-only guest disk.
///
/// The image can hold private keys, so `host` writes it as a private file ([`Host::write_private`]). An image left at
/// `into` by an earlier launch is replaced.
///
/// The volume is FAT12, labelled `CIDATA`. Measured: Ubuntu 24.04's cloud-init reads a FAT12 image of about 1 MiB
/// attached read-only on `virtio-blk-pci`, as the QEMU backend attaches it. Files too large for a FAT12 volume are
/// refused with `Internal` before anything is allocated, never written in another FAT type.
///
/// The image is a pure function of `files`: no timestamps, a fixed volume id. The names and contents are read back from
/// the finished image before anything is written; an image that does not hold exactly `files` is an error.
pub(crate) fn write_cidata_image(host: &dyn Host, files: &[SeedFile], into: &Path) -> Result<Media> {
    refuse_path_names(files)?;
    let image = fat_image(files, into)?;
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
        kind: MediaKind::Disk,
        boot_index: None,
        readonly: true,
    })
}

/// `fatfs` trims `/` from a name and walks the rest as a path, so `a/` is `a` and `a/b` needs a directory; `.` and `..`
/// are no files. Names that merely collide or that FAT cannot store are caught by [`require_files_as_asked`].
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

fn build_failed(error: &std::io::Error) -> Error {
    Error::new(ErrorKind::Internal, format!("cannot build the seed image: {error}"))
}

/// The size in bytes of the image that holds `contents` bytes of files, `Internal` when that is more than a FAT12
/// volume holds ([`LARGEST_FAT12_CONTENTS`]). Decided from the sum alone, so nothing is allocated for a refused seed.
fn image_size(contents: u64) -> Result<u64> {
    if contents > LARGEST_FAT12_CONTENTS {
        let size = (u128::from(MIN_IMAGE) + 2 * u128::from(contents)).next_multiple_of(u128::from(SECTOR));
        return Err(Error::new(
            ErrorKind::Internal,
            format!(
                "the seed files ({contents} bytes) need an image of {size} bytes, which would not be FAT12: \
                 at most {LARGEST_FAT12_CONTENTS} bytes of files fit"
            ),
        ));
    }
    Ok((MIN_IMAGE + 2 * contents).next_multiple_of(u64::from(SECTOR)))
}

fn fat_image(files: &[SeedFile], into: &Path) -> Result<Vec<u8>> {
    let io = |e: std::io::Error| {
        Error::io(
            ErrorKind::Io,
            format!("cannot build the seed image {}", into.display()),
            e,
        )
    };
    let contents: u64 = files.iter().map(|file| file.bytes.len() as u64).sum();
    let size = image_size(contents)?;
    // Fits `u32`: see the assertion beside `LARGEST_FAT12_CONTENTS`.
    let total_sectors = (size / u64::from(SECTOR)) as u32;

    let mut disk = Cursor::new(vec![0u8; size as usize]);
    fatfs::format_volume(
        &mut disk,
        FormatVolumeOptions::new()
            .bytes_per_sector(SECTOR as u16)
            .total_sectors(total_sectors)
            .fat_type(FatType::Fat12)
            .volume_id(VOLUME_ID)
            .volume_label(*b"CIDATA     "),
    )
    .map_err(|e| build_failed(&e))?;
    {
        let fs = FileSystem::new(&mut disk, FsOptions::new()).map_err(io)?;
        if fs.fat_type() != FatType::Fat12 {
            return Err(Error::new(
                ErrorKind::Internal,
                format!("the seed image was formatted as {:?}, not FAT12", fs.fat_type()),
            ));
        }
        for file in files {
            let mut out = fs.root_dir().create_file(&file.name).map_err(|e| {
                if is_name_rejection(&e) {
                    unusable_name(&file.name, &e.to_string())
                } else {
                    build_failed(&e)
                }
            })?;
            out.write_all(&file.bytes).map_err(io)?;
            out.flush().map_err(io)?;
        }
        fs.unmount().map_err(io)?;
    }
    require_files_as_asked(&mut disk, files, io)?;
    Ok(disk.into_inner())
}

/// Reads the root directory of the finished image back and requires exactly `files`: the same long names, the same
/// count, the same contents.
///
/// `fatfs` merges names whose generated 8.3 short names, trailing dots or U+FFFF padding collide, and renames some, so
/// no list of name patterns closes the class; only the image says what it holds.
fn require_files_as_asked(
    disk: &mut Cursor<Vec<u8>>,
    files: &[SeedFile],
    io: impl Fn(std::io::Error) -> Error,
) -> Result<()> {
    disk.set_position(0);
    let fs = FileSystem::new(disk, FsOptions::new()).map_err(&io)?;
    let mut held = Vec::new();
    for entry in fs.root_dir().iter() {
        let entry = entry.map_err(&io)?;
        let mut bytes = Vec::new();
        if entry.is_file() {
            entry.to_file().read_to_end(&mut bytes).map_err(&io)?;
        }
        held.push((entry.file_name(), bytes));
    }

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
