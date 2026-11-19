//! Windows installer ISOs as the root of a recipe: the hash a layer key is made from, the image a build installs, and
//! the one change an install needs in the ISO's boot image.
//!
//! The ISO stays where the recipe says; devvm never copies it. A build holds it open from keying until its root layer
//! is found or built ([`HeldIso`]) and reads it through its UDF tree. Only a Windows host can deny other programs the
//! file; devvm's host is Windows.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::host::{Host, is_absent};
use crate::progress::Progress;
use crate::recipe::{GenericKey, ProductKey};
use crate::text::escape_controls;
use crate::{Error, ErrorKind, Result, Sha256};

mod memo;
mod udf;
mod wim;

pub(crate) use memo::{HashedIso, IsoMemo, Known, QuarantineOutcome};
pub(crate) use wim::{WimArch, WimImage, names_an_evaluation};

use udf::{BootEntry, Piece, Platform, SECTOR, Udf};

#[cfg(test)]
#[path = "iso_tests.rs"]
mod iso_tests;

/// The installation files of an installer ISO, in the order Setup looks for them.
const INSTALL_FILES: [&str; 2] = ["install.wim", "install.esd"];
/// The boot image that waits for a key press (`Press any key to boot from CD or DVD`), and the one that does not.
const PROMPTING_IMAGE: &str = "/efi/microsoft/boot/efisys.bin";
const NO_PROMPT_IMAGE: &str = "/efi/microsoft/boot/efisys_noprompt.bin";
/// Where Microsoft lists the generic key of each edition it has one for.
const KMS_KEYS_PAGE: &str = "https://learn.microsoft.com/en-us/windows-server/get-started/kms-client-activation-keys";

/// Bytes to write at an offset of the ISO's image: the same shape as `backend::Patch`, which the build hands to
/// `Storage::overlay_with_patches`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BootPatch {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// An installer ISO, opened with [`Host::open_held_read`] and kept for the build, so what is read is the file that was
/// hashed and keyed.
///
/// The cursor is not meant to be relied on: read through `try_clone`d handles that seek first, as [`Udf`] does.
#[derive(Debug)]
pub(crate) struct HeldIso {
    path: PathBuf,
    file: File,
}

impl HeldIso {
    /// Opens and holds the ISO at `path`; a missing one is `NotFound`, and one that another program is writing is an
    /// error naming the ISO and why.
    pub(crate) fn open(host: &dyn Host, path: &Path) -> Result<HeldIso> {
        let file = host.open_held_read(path).map_err(|e| {
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
            file,
        })
    }

    /// The path the ISO was opened by, for messages.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The path of the held file with no link in it ([`Host::final_path_of_open`]), for a program that opens the ISO by
    /// name (QEMU): the name the ISO was opened by can be pointed at another file while the hold lasts, by re-making a
    /// junction or a symlink in it, and this path cannot. It is asked for, and fails alone, where the hold does not.
    pub(crate) fn final_path(&self, host: &dyn Host) -> Result<PathBuf> {
        host.final_path_of_open(&self.file, &self.path).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot find the path of {}", self.path.display()),
                e,
            )
        })
    }

    pub(crate) fn file(&self) -> &File {
        &self.file
    }
}

/// What a build reads from an ISO: the keyed [`IsoRoot`] and the boot patch. Built only by [`read_install_iso`], so
/// image, key and evaluation flag belong to one image. `key` is `None` only for an evaluation edition.
#[derive(Debug)]
pub(crate) struct InstallIso {
    root: IsoRoot,
    boot_patches: Vec<BootPatch>,
}

impl InstallIso {
    /// The ISO, still held until the guest's disk has opened it. Reopen it by [`HeldIso::final_path`], not the recipe's
    /// path.
    pub(crate) fn iso(&self) -> &HeldIso {
        &self.root.iso
    }

    /// Gives up the held ISO, for the build to hold until the guest's disk has opened it.
    pub(crate) fn into_iso(self) -> HeldIso {
        self.root.iso
    }

    /// The image the recipe's `edition` selected (the only one when the ISO has one).
    pub(crate) fn image(&self) -> &WimImage {
        self.root.image()
    }

    /// The key Setup installs [`InstallIso::image`] with.
    pub(crate) fn product_key(&self) -> Option<&ProductKey> {
        self.root.product_key()
    }

    /// Whether [`InstallIso::image`] is an evaluation edition.
    pub(crate) fn evaluation(&self) -> bool {
        self.root.image().is_evaluation()
    }

    /// Writing these over the ISO makes it boot the installer without a key press: the no-prompt UEFI boot image goes
    /// where the boot catalog's UEFI entry loads its image.
    pub(crate) fn boot_patches(&self) -> &[BootPatch] {
        &self.boot_patches
    }
}

/// What a layer key is made from, of an ISO, and the ISO held. Built only by [`key_iso`], which resolves the key for
/// the image it selects.
#[derive(Debug)]
pub(crate) struct IsoRoot {
    sha256: Sha256,
    image: WimImage,
    product_key: Option<ProductKey>,
    iso: HeldIso,
}

impl IsoRoot {
    /// The SHA-256 of the ISO's content.
    pub(crate) fn sha256(&self) -> Sha256 {
        self.sha256
    }

    /// The image the recipe's `edition` selects: what the build installs, so what the key says.
    pub(crate) fn image(&self) -> &WimImage {
        &self.image
    }

    /// The key Setup installs [`IsoRoot::image`] with, as [`resolve_product_key`] resolved it when the ISO was keyed.
    pub(crate) fn product_key(&self) -> Option<&ProductKey> {
        self.product_key.as_ref()
    }
}

/// The content hash of the installer ISO at `iso`, the image `edition` selects in it (refusing one that is not x64)
/// and the key that image is installed with ([`resolve_product_key`] of the recipe's `product_key`), with the ISO
/// held. A hash and an image list the memo has for the ISO are used while the ISO is unchanged; otherwise the ISO is
/// read once, and the memo remembers both.
pub(crate) fn key_iso(
    host: &dyn Host,
    memo: &IsoMemo,
    iso: &Path,
    edition: Option<&str>,
    product_key: Option<&ProductKey>,
    progress: &dyn Progress,
) -> Result<IsoRoot> {
    let HashedIso { iso, sha256, images } = memo.open(host, iso, progress, &read_images)?;
    let image = select_image(&images, edition)?.clone();
    let product_key = resolve_product_key(&image, product_key)?;
    Ok(IsoRoot {
        sha256,
        image,
        product_key,
        iso,
    })
}

/// What the build installs from the ISO that `root` keyed: the no-prompt boot image and where the boot catalog loads its
/// UEFI image.
pub(crate) fn read_install_iso(root: IsoRoot) -> Result<InstallIso> {
    debug_assert_eq!(
        root.image.is_evaluation(),
        root.product_key.is_none(),
        "a root has no product key exactly when its image is an evaluation edition"
    );
    let boot_patches = vec![no_prompt_patch(&mount(&root.iso)?, root.iso.path())?];
    Ok(InstallIso { root, boot_patches })
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
            .map(|image| format!("`{}`", escape_controls(&image.name)))
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
                        format!(
                            "the ISO has no edition `{}`; its editions are {}",
                            escape_controls(edition),
                            names()
                        ),
                    ));
                }
                [one] => one,
                ref several => {
                    let indexes: Vec<String> = several.iter().map(|image| image.index.to_string()).collect();
                    return Err(Error::new(
                        ErrorKind::Recipe,
                        format!(
                            "the ISO has several images named `{}` (images {}); devvm cannot tell them apart",
                            escape_controls(edition),
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
                escape_controls(&chosen.name),
                chosen.arch.describe()
            ),
        ));
    }
    Ok(chosen)
}

/// The key the build installs `image` with: the recipe's `product_key`, else the generic key for the image's edition and
/// build ([`ProductKey::generic_for`]). Refuses a key given to an evaluation edition, and no key for any other edition,
/// naming `product-key` and Microsoft's page.
fn resolve_product_key(image: &WimImage, product_key: Option<&ProductKey>) -> Result<Option<ProductKey>> {
    let name = escape_controls(&image.name);
    if image.is_evaluation() {
        return match product_key {
            None => Ok(None),
            Some(_) => Err(Error::new(
                ErrorKind::Recipe,
                format!("the edition `{name}` is an evaluation edition; `product-key` in `from:` must not be set"),
            )),
        };
    }
    if let Some(key) = product_key {
        return Ok(Some(key.clone()));
    }
    let build = image.build_number();
    let lookup = build.map(|build| ProductKey::generic_for(&image.edition_id, build, image.spbuild_number()));
    let id = escape_controls(&image.edition_id);
    let detail = match (&image.edition_id[..], image.build.as_deref(), build, lookup) {
        (_, _, _, Some(GenericKey::Found(key))) => return Ok(Some(key)),
        ("", ..) => "no edition id".to_owned(),
        (_, None, ..) => format!("edition id `{id}`, no build"),
        (_, Some(text), None, _) => format!(
            "edition id `{id}`, BUILD `{}` is not a build number devvm can read",
            escape_controls(text)
        ),
        (_, Some(_), Some(build), Some(GenericKey::Gated { first_spbuild })) => {
            let spbuild = match (image.spbuild.as_deref(), image.spbuild_number()) {
                (None, _) => ", the image names no SPBUILD".to_owned(),
                (Some(_), Some(spbuild)) => format!(".{spbuild}"),
                (Some(text), None) => format!(
                    ", SPBUILD `{}` is not a build number devvm can read",
                    escape_controls(text)
                ),
            };
            format!(
                "edition id `{id}`, build {build}{spbuild}; its generic key needs media of {build}.{first_spbuild} or later"
            )
        }
        (_, Some(_), Some(build), _) => format!("edition id `{id}`, build {build}"),
    };
    Err(Error::new(
        ErrorKind::Recipe,
        format!(
            "devvm has no generic product key for the edition `{name}` ({detail}); set `product-key` in `from:`; Microsoft's list is {KMS_KEYS_PAGE}"
        ),
    ))
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
