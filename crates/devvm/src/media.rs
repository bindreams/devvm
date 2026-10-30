//! Writing the small disk images a guest reads at first boot.

use std::io::{Cursor, Write};
use std::path::Path;

use fatfs::{FileSystem, FormatVolumeOptions, FsOptions};

use crate::backend::{Media, MediaKind};
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
/// A FAT volume labelled `CIDATA` is read by cloud-init's NoCloud source as well as an ISO9660 one, and needs no ISO
/// writer. It is attached as a virtio-blk disk. Unmeasured: that Ubuntu 24.04 mounts it that way; the first guest boot
/// decides, and the fallback is an ISO9660 image with Joliet and Rock Ridge names (the format measured as an `ide-cd`).
///
/// The image is a pure function of `files`: no timestamps, a fixed volume id.
pub(crate) fn write_cidata_image(files: &[SeedFile], into: &Path) -> Result<Media> {
    let image = fat_image(files).map_err(|e| {
        Error::io(
            ErrorKind::Io,
            format!("cannot build the seed image {}", into.display()),
            e,
        )
    })?;
    std::fs::write(into, image).map_err(|e| {
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

fn fat_image(files: &[SeedFile]) -> std::io::Result<Vec<u8>> {
    let contents: u64 = files.iter().map(|file| file.bytes.len() as u64).sum();
    let size = (MIN_IMAGE + 2 * contents).next_multiple_of(u64::from(SECTOR));
    let total_sectors = u32::try_from(size / u64::from(SECTOR)).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the seed files do not fit a FAT volume",
        )
    })?;

    let mut disk = Cursor::new(vec![0u8; size as usize]);
    fatfs::format_volume(
        &mut disk,
        FormatVolumeOptions::new()
            .bytes_per_sector(SECTOR as u16)
            .total_sectors(total_sectors)
            .volume_id(VOLUME_ID)
            .volume_label(*b"CIDATA     "),
    )?;
    {
        let fs = FileSystem::new(&mut disk, FsOptions::new())?;
        for file in files {
            let mut out = fs.root_dir().create_file(&file.name)?;
            out.write_all(&file.bytes)?;
            out.flush()?;
        }
        fs.unmount()?;
    }
    Ok(disk.into_inner())
}
