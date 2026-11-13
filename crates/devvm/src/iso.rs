//! Windows installer ISOs as the root of a recipe: the hash a layer key is made from, the image a build installs, and
//! the one change an install needs in the ISO's boot image.
//!
//! The ISO stays where the recipe says; devvm never copies it. A build holds it open from keying until its root layer
//! is found or built ([`HeldIso`]) and reads it through its UDF tree. The hold is a Windows host's: devvm's host is
//! Windows, and no other host denies other programs the file.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::exit::Progress;
use crate::host::{HeldFile, Host, is_absent};
use crate::{Error, ErrorKind, Result, Sha256};

mod memo;
mod udf;
mod wim;

pub(crate) use memo::{HashedIso, IsoMemo, Known, QuarantineOutcome};
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

/// An installer ISO, open and held: on a Windows host, while this lives, no other program can write the ISO, delete it,
/// rename it or rename another file onto its name ([`Host::open_held_read`]), so the file read through it is the file
/// that was hashed, keyed and looked at, whatever happens to its path. Anything else that reads the ISO, QEMU included,
/// opens it read-only as usual. Other hosts open it as any read.
///
/// The cursor is not meant to be relied on: read through `try_clone`d handles that seek first, as [`Udf`] does.
#[derive(Debug)]
pub(crate) struct HeldIso {
    path: PathBuf,
    final_path: PathBuf,
    file: File,
}

impl HeldIso {
    /// Opens and holds the ISO at `path`; a missing one is `NotFound`, and one that another program is writing is an
    /// error naming the ISO and why.
    pub(crate) fn open(host: &dyn Host, path: &Path) -> Result<HeldIso> {
        let HeldFile { file, final_path } = host.open_held_read(path).map_err(|e| {
            if is_absent(&e) {
                Error::new(
                    ErrorKind::NotFound,
                    format!("the ISO {} does not exist", path.display()),
                )
                .with_source(e)
            } else {
                Error::io(ErrorKind::Io, format!("cannot open {}", path.display()), e)
            }
        })?;
        Ok(HeldIso {
            path: path.to_owned(),
            final_path,
            file,
        })
    }

    /// The path the ISO was opened by, for messages.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The path of the held file with no link in it, for a program that opens the ISO by name (QEMU): the name the ISO
    /// was opened by can be pointed at another file while the hold lasts, by re-making a junction or a symlink in it,
    /// and this path cannot.
    pub(crate) fn final_path(&self) -> &Path {
        &self.final_path
    }

    pub(crate) fn file(&self) -> &File {
        &self.file
    }
}

/// What a build reads from an ISO.
#[derive(Debug)]
pub(crate) struct InstallIso {
    /// The ISO, still held. The build keeps it until the guest's disk has opened the ISO itself: W4b gives QEMU
    /// [`HeldIso::final_path`], which no re-made link can point elsewhere, and QEMU opens it read-only, which the hold
    /// allows. Reopening the ISO by the recipe's path reads whatever the path leads to by then.
    pub iso: HeldIso,
    /// The image the recipe's `edition` selected (the only one when the ISO has one).
    pub image: WimImage,
    /// Writing these over the ISO makes it boot the installer without a key press: the no-prompt UEFI boot image goes
    /// where the boot catalog's UEFI entry loads its image.
    pub boot_patches: Vec<BootPatch>,
}

/// What a layer key is made from, of an ISO, and the ISO held.
#[derive(Debug)]
pub(crate) struct IsoRoot {
    /// The SHA-256 of the ISO's content.
    pub sha256: Sha256,
    /// The image the recipe's `edition` selects: what the build installs, so what the key says.
    pub image: WimImage,
    iso: HeldIso,
}

/// The content hash of the installer ISO at `iso` and the image `edition` selects in it (refusing one that is not x64),
/// with the ISO held. A hash and an image list the memo has for the ISO are used while the ISO is unchanged; otherwise
/// the ISO is read once, and the memo remembers both.
pub(crate) fn key_iso(
    host: &dyn Host,
    memo: &IsoMemo,
    iso: &Path,
    edition: Option<&str>,
    progress: &dyn Progress,
) -> Result<IsoRoot> {
    let HashedIso { iso, sha256, images } = memo.open(host, iso, progress, &read_images)?;
    let image = select_image(&images, edition)?.clone();
    Ok(IsoRoot { sha256, image, iso })
}

/// What the build installs from the ISO that `root` keyed: the no-prompt boot image and where the boot catalog loads its
/// UEFI image. Nothing is written. Everything is read through the handle `root` holds, and `InstallIso` carries it on.
pub(crate) fn read_install_iso(root: IsoRoot) -> Result<InstallIso> {
    let IsoRoot { image, iso, .. } = root;
    let boot_patches = vec![no_prompt_patch(&mount(&iso)?, iso.path())?];
    Ok(InstallIso {
        iso,
        image,
        boot_patches,
    })
}

/// The UDF tree of `iso`, read through its handle.
fn mount(iso: &HeldIso) -> Result<Udf<'_>> {
    let file = iso.file().try_clone().map_err(|e| {
        Error::io(
            ErrorKind::Io,
            format!("cannot open {} again to read it", iso.path().display()),
            e,
        )
    })?;
    Udf::open(iso.path(), file)
}

/// The images of the ISO's installation file, `install.wim` or else `install.esd`.
fn read_images(held: &HeldIso) -> Result<Vec<WimImage>> {
    let (udf, iso) = (mount(held)?, held.path());
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

/// The patch that puts the no-prompt boot image where the UEFI entry of the boot catalog loads its image. The image
/// the catalog loads is a copy of `efisys.bin`; it is replaced only when it is that.
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
    let loaded = Piece {
        offset: u64::from(entry.load_block) * SECTOR,
        len: prompting_size,
    };
    if udf.read_pieces(&[loaded])? != udf.read_pieces(&prompting)? {
        return Err(Error::new(
            ErrorKind::Recipe,
            format!(
                "the UEFI boot image of {} (block {}) is not {}, so {} cannot stand in for it",
                iso.display(),
                entry.load_block,
                file_name(PROMPTING_IMAGE),
                file_name(NO_PROMPT_IMAGE)
            ),
        ));
    }
    Ok(BootPatch {
        offset: loaded.offset,
        bytes: udf.read_pieces(&no_prompt)?,
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

/// The last part of the `/`-separated `path`.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
