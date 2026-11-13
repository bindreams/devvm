//! Windows installer ISOs as the root of a recipe: the hash a layer key is made from, the image a build installs, and
//! the one change an install needs in the ISO's boot image.
//!
//! The ISO stays where the recipe says; devvm never copies it. A build reads it through its UDF tree.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::exit::Progress;
use crate::host::Host;
use crate::{Error, ErrorKind, Result, Sha256};

mod memo;
mod udf;
mod wim;

pub(crate) use memo::{IsoMemo, Known};
pub(crate) use wim::{WimArch, WimImage};

use udf::{BootEntry, Piece, Platform, SECTOR, Udf};

#[cfg(test)]
#[path = "iso_tests.rs"]
mod iso_tests;

/// The installation files of an installer ISO, in the order Setup looks for them.
const INSTALL_FILES: [&str; 2] = ["install.wim", "install.esd"];
/// The boot image that waits for a key press (`Press any key to boot from CD or DVD`), and the one that does not.
const PROMPTING_IMAGE: &str = "/efi/microsoft/boot/efisys.bin";
const NO_PROMPT_IMAGE: &str = "/efi/microsoft/boot/efisys_noprompt.bin";

/// Bytes to write at an offset of the ISO's image: the same shape as `backend::Patch`, which the build hands to
/// `Storage::overlay_with_patches`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BootPatch {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// What a build reads from an ISO.
#[derive(Debug)]
pub(crate) struct InstallIso {
    /// The image the recipe's `edition` selected (the only one when the ISO has one).
    pub image: WimImage,
    /// Writing these over the ISO makes it boot the installer without a key press: the no-prompt UEFI boot image goes
    /// where the boot catalog's UEFI entry loads its image.
    pub boot_patches: Vec<BootPatch>,
}

/// Reads the installer ISO at `iso`, which the layer key was made from (`key_hash`, as [`IsoMemo::sha256`] gave it):
/// fails, naming the ISO, when it is no longer that file. Selects the image `edition` names, refuses an image that is
/// not x64, and reads the no-prompt boot image and where the boot catalog loads its UEFI image. Nothing is written.
pub(crate) fn read_install_iso(
    host: &dyn Host,
    memo: &IsoMemo,
    iso: &Path,
    key_hash: Sha256,
    edition: Option<&str>,
    progress: &dyn Progress,
) -> Result<InstallIso> {
    memo.check_unchanged(host, iso, key_hash, progress)?;
    let udf = Udf::open(iso)?;
    let images = read_images(&udf, iso)?;
    let image = select_image(&images, edition)?.clone();
    let boot_patches = vec![no_prompt_patch(&udf, iso)?];
    Ok(InstallIso { image, boot_patches })
}

/// The images of the ISO's installation file, `install.wim` or else `install.esd`.
fn read_images(udf: &Udf<'_>, iso: &Path) -> Result<Vec<WimImage>> {
    for name in INSTALL_FILES {
        if let Some(mut file) = udf.open_file(&format!("/sources/{name}"))? {
            return wim::read_image_list(&mut file, name);
        }
    }
    Err(Error::new(
        ErrorKind::Recipe,
        format!(
            "{} has neither sources/{} nor sources/{}, so it is not a Windows installer ISO",
            iso.display(),
            INSTALL_FILES[0],
            INSTALL_FILES[1]
        ),
    ))
}

/// The image of `images` that a recipe's `edition` selects. The edition is optional when there is one image, and when
/// it is given it must match. The image must be x64.
pub(crate) fn select_image<'a>(images: &'a [WimImage], edition: Option<&str>) -> Result<&'a WimImage> {
    let names = || {
        images
            .iter()
            .map(|image| format!("`{}`", image.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let chosen = match (edition, images) {
        (_, []) => return Err(Error::new(ErrorKind::Recipe, "the ISO has no Windows image")),
        (None, [only]) => only,
        (None, _) => {
            return Err(Error::new(
                ErrorKind::Recipe,
                format!(
                    "the ISO has several editions: {}; name one with `edition:` in `from:`",
                    names()
                ),
            ));
        }
        (Some(edition), _) => {
            let matching: Vec<&WimImage> = images.iter().filter(|image| image.name == edition).collect();
            match matching[..] {
                [] => {
                    return Err(Error::new(
                        ErrorKind::Recipe,
                        format!("the ISO has no edition `{edition}`; its editions are {}", names()),
                    ));
                }
                [one] => one,
                ref several => {
                    let indexes: Vec<String> = several.iter().map(|image| image.index.to_string()).collect();
                    return Err(Error::new(
                        ErrorKind::Recipe,
                        format!(
                            "the ISO has several images named `{edition}` (images {}); devvm cannot tell them apart",
                            indexes.join(", ")
                        ),
                    ));
                }
            }
        }
    };
    if chosen.arch != WimArch::X64 {
        return Err(Error::new(
            ErrorKind::NotSupported,
            format!(
                "image {} `{}` is {}; devvm builds x64 Windows guests only",
                chosen.index,
                chosen.name,
                chosen.arch.describe()
            ),
        ));
    }
    Ok(chosen)
}

/// The patch that puts the no-prompt boot image where the UEFI entry of the boot catalog loads its image.
fn no_prompt_patch(udf: &Udf<'_>, iso: &Path) -> Result<BootPatch> {
    let find = |path: &str| {
        udf.pieces(path)?.ok_or_else(|| {
            Error::new(
                ErrorKind::Recipe,
                format!(
                    "{} has no {}, which the installer boots from without a key press",
                    iso.display(),
                    path.trim_start_matches('/')
                ),
            )
        })
    };
    let prompting = find(PROMPTING_IMAGE)?;
    let no_prompt = find(NO_PROMPT_IMAGE)?;
    let (prompting_size, no_prompt_size) = (total(&prompting), total(&no_prompt));
    if prompting_size != no_prompt_size {
        return Err(Error::new(
            ErrorKind::Recipe,
            format!(
                "the boot images of {} differ in size ({} is {prompting_size} bytes, {} is {no_prompt_size} bytes), so one cannot stand in for the other",
                iso.display(),
                PROMPTING_IMAGE.trim_start_matches('/'),
                NO_PROMPT_IMAGE.trim_start_matches('/')
            ),
        ));
    }
    let entries = udf.boot_entries()?;
    let entry = efi_entry(&entries).ok_or_else(|| {
        Error::new(
            ErrorKind::Recipe,
            format!("the boot catalog of {} has no bootable UEFI entry", iso.display()),
        )
    })?;
    Ok(BootPatch {
        offset: u64::from(entry.load_block) * SECTOR,
        bytes: read_pieces(iso, &no_prompt)?,
    })
}

fn efi_entry(entries: &[BootEntry]) -> Option<&BootEntry> {
    entries
        .iter()
        .find(|entry| entry.platform == Platform::Efi && entry.bootable)
}

fn total(pieces: &[Piece]) -> u64 {
    pieces.iter().map(|piece| piece.len).sum()
}

/// The bytes of `pieces` of the file `iso`, one after the other.
fn read_pieces(iso: &Path, pieces: &[Piece]) -> Result<Vec<u8>> {
    let fail = |e| Error::io(ErrorKind::Io, format!("cannot read {}", iso.display()), e);
    let mut file = std::fs::File::open(iso).map_err(fail)?;
    let mut bytes = Vec::with_capacity(total(pieces) as usize);
    for piece in pieces {
        file.seek(SeekFrom::Start(piece.offset)).map_err(fail)?;
        let start = bytes.len();
        bytes.resize(start + piece.len as usize, 0);
        file.read_exact(&mut bytes[start..]).map_err(fail)?;
    }
    Ok(bytes)
}
