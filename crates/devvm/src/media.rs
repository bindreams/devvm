//! Writing the small disk images a guest reads at first boot.

use std::io::{Cursor, Read, Write};
use std::path::Path;

use fatfs::{FileSystem, FormatVolumeOptions, FsOptions};

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
/// The volume id of every image: a fixed value keeps equal inputs byte-identical.
const VOLUME_ID: u32 = 0x4456_4d53;

/// Writes the NoCloud `cidata` image holding `files` to `into` and returns it as a read-only guest disk.
///
/// The image can hold private keys, so `host` writes it as a private file ([`Host::write_private`]). An image left at
/// `into` by an earlier launch is replaced.
///
/// A FAT volume labelled `CIDATA` is read by cloud-init's NoCloud source as well as an ISO9660 one, and needs no ISO
/// writer. It is attached as a virtio-blk disk. Unmeasured: that Ubuntu 24.04 mounts it that way; the first guest boot
/// decides, and the fallback is an ISO9660 image with Joliet and Rock Ridge names (the format measured as an `ide-cd`).
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

fn fat_image(files: &[SeedFile], into: &Path) -> Result<Vec<u8>> {
    let io = |e: std::io::Error| {
        Error::io(
            ErrorKind::Io,
            format!("cannot build the seed image {}", into.display()),
            e,
        )
    };
    let contents: u64 = files.iter().map(|file| file.bytes.len() as u64).sum();
    let size = (MIN_IMAGE + 2 * contents).next_multiple_of(u64::from(SECTOR));
    let total_sectors = u32::try_from(size / u64::from(SECTOR)).map_err(|_| {
        io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the seed files do not fit a FAT volume",
        ))
    })?;

    let mut disk = Cursor::new(vec![0u8; size as usize]);
    fatfs::format_volume(
        &mut disk,
        FormatVolumeOptions::new()
            .bytes_per_sector(SECTOR as u16)
            .total_sectors(total_sectors)
            .volume_id(VOLUME_ID)
            .volume_label(*b"CIDATA     "),
    )
    .map_err(io)?;
    {
        let fs = FileSystem::new(&mut disk, FsOptions::new()).map_err(io)?;
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
