use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256 as Sha2};

use super::udf::udf_tests::{image, read_at};
use super::{
    BootPatch, HeldIso, InstallIso, IsoMemo, IsoRoot, NO_PROMPT_IMAGE, PROMPTING_IMAGE, SECTOR, WimArch, WimImage,
    efi_entry, key_iso, mount, no_prompt_patch, read_install_iso, resolve_product_key, select_image, total,
};
use crate::recipe::ProductKey;
use crate::test_groups::{Group, WINDOWS_ISO_VAR, windows_guest};
use crate::test_support::fixtures::{MemoryProgress, iso_fixture, unicode_home};
use crate::test_support::host::TestHost;
use crate::{Dirs, ErrorKind, Result, Sha256};

#[cfg(windows)]
#[path = "iso_windows_tests.rs"]
mod iso_windows_tests;

/// A copy of the fixture ISO `name` in `home` (under a name with a space, Cyrillic and a surrogate pair), changed by
/// `edit`.
fn copy_of(home: &Path, name: &str, edit: impl FnOnce(&Path)) -> PathBuf {
    let iso = home.join(format!(
        "{name} \u{43f}\u{440}\u{43e}\u{431}\u{435}\u{43b} \u{1f600}.iso"
    ));
    std::fs::copy(iso_fixture(name), &iso).unwrap();
    edit(&iso);
    iso
}

fn key(home: &Path, iso: &Path, edition: Option<&str>) -> Result<IsoRoot> {
    let memo = IsoMemo::new(&Dirs::under(home));
    key_iso(&TestHost, &memo, iso, edition, None, &MemoryProgress::default())
}

/// The ISO fixture `name` keyed and prepared: what a build does with it.
fn read(home: &Path, name: &str, edition: Option<&str>) -> (PathBuf, Result<InstallIso>) {
    read_after(home, name, edition, |_| {})
}

/// `read`, with `edit` changing the copy first.
fn read_after(
    home: &Path,
    name: &str,
    edition: Option<&str>,
    edit: impl FnOnce(&Path),
) -> (PathBuf, Result<InstallIso>) {
    let iso = copy_of(home, name, edit);
    let result = key(home, &iso, edition).and_then(read_install_iso);
    (iso, result)
}

fn image_of(index: u32, name: &str, arch: WimArch) -> WimImage {
    WimImage {
        index,
        name: name.to_owned(),
        edition_id: String::new(),
        build: None,
        arch,
        language: "en-US".to_owned(),
    }
}

// The product key =====================================================================================================

const KEY: &str = "AB12C-DE34F-GH56J-KL78M-NP90Q";

fn edition_of(id: &str, name: &str) -> WimImage {
    WimImage {
        edition_id: id.to_owned(),
        ..image_of(1, name, WimArch::X64)
    }
}

fn built(build: u32, image: WimImage) -> WimImage {
    WimImage {
        build: Some(build),
        ..image
    }
}

const KMS_PAGE: &str = "https://learn.microsoft.com/en-us/windows-server/get-started/kms-client-activation-keys";

#[skuld::test]
fn the_recipes_product_key_is_used_for_any_edition_that_is_not_an_evaluation() {
    let key = ProductKey::parse(KEY).unwrap();
    for image in [
        built(
            26100,
            edition_of("IoTEnterpriseS", "Windows 11 IoT Enterprise LTSC 2024"),
        ),
        built(
            26100,
            edition_of("IoTEnterpriseSK", "an edition the table does not have"),
        ),
        edition_of("", "No id"),
    ] {
        assert_eq!(
            resolve_product_key(&image, Some(&key)).unwrap(),
            Some(key.clone()),
            "{image:?}"
        );
    }
}

#[skuld::test]
fn an_edition_with_no_key_in_the_recipe_gets_the_generic_key_of_its_edition_and_build() {
    let ltsc = built(
        26100,
        edition_of("IoTEnterpriseS", "Windows 11 IoT Enterprise LTSC 2024"),
    );
    let generic = ProductKey::generic_for("IoTEnterpriseS", Some(26100)).unwrap();
    assert_eq!(resolve_product_key(&ltsc, None).unwrap(), Some(generic));
    // The key follows the build: one edition id, three keys.
    let key = |build| resolve_product_key(&built(build, edition_of("EnterpriseS", "e")), None).unwrap();
    assert!(key(10240).is_some() && key(14393).is_some() && key(17763).is_some());
    assert_ne!(key(10240), key(14393));
    assert_ne!(key(14393), key(17763));
}

#[skuld::test]
fn an_evaluation_edition_takes_no_key_from_the_recipe_or_the_table() {
    let key = ProductKey::parse(KEY).unwrap();
    let eval = built(26100, edition_of("EnterpriseEval", "Windows 11 Enterprise Evaluation"));
    assert_eq!(resolve_product_key(&eval, None).unwrap(), None);

    let error = resolve_product_key(&eval, Some(&key)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert_eq!(
        error.message(),
        "the edition `Windows 11 Enterprise Evaluation` is an evaluation edition; `product-key` in `from:` must not be set"
    );
    assert!(!format!("{error:?} {error}").contains(KEY));
}

#[skuld::test]
fn an_edition_with_no_key_anywhere_is_refused_naming_product_key_and_microsofts_page() {
    for (image, detail) in [
        (
            built(
                26100,
                edition_of("IoTEnterpriseSK", "Windows 11 IoT Enterprise Subscription LTSC 2024"),
            ),
            "edition id `IoTEnterpriseSK`, build 26100",
        ),
        (
            built(28000, edition_of("EnterpriseS", "Windows 12 Enterprise LTSC")),
            "edition id `EnterpriseS`, build 28000",
        ),
        (
            edition_of("EnterpriseS", "No build"),
            "edition id `EnterpriseS`, no build",
        ),
        (edition_of("", "No id"), "no edition id"),
    ] {
        let error = resolve_product_key(&image, None).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Recipe);
        assert_eq!(
            error.message(),
            format!(
                "devvm has no generic product key for the edition `{}` ({detail}); set `product-key` in `from:`; Microsoft's list is {KMS_PAGE}",
                image.name
            )
        );
    }
}

#[skuld::test]
fn a_keyed_iso_root_has_the_key_its_image_is_installed_with(#[fixture(unicode_home)] home: &Path) {
    let memo = IsoMemo::new(&Dirs::under(home));
    let (three, one) = (
        copy_of(home, "three-images.iso", |_| {}),
        copy_of(home, "single-image.iso", |_| {}),
    );
    let root = |iso: &Path, edition: Option<&str>, key: Option<&ProductKey>| {
        key_iso(&TestHost, &memo, iso, edition, key, &MemoryProgress::default())
    };
    let given = ProductKey::parse(KEY).unwrap();
    let ltsc = Some("Windows 11 IoT Enterprise LTSC 2024");
    let generic = ProductKey::generic_for("IoTEnterpriseS", Some(26100));
    assert_eq!(root(&three, ltsc, None).unwrap().product_key, generic);
    assert_eq!(
        root(&three, ltsc, Some(&given)).unwrap().product_key,
        Some(given.clone())
    );
    assert_eq!(root(&one, None, None).unwrap().product_key, None);
    assert!(
        root(&one, None, Some(&given)).is_err(),
        "an evaluation edition takes no key"
    );
    assert!(root(&three, Some("Windows 11 IoT Enterprise Subscription LTSC 2024"), None).is_err());
}

// The boot patch ======================================================================================================

#[skuld::test]
fn the_boot_catalog_extent_ends_up_holding_the_no_prompt_image_bytes(#[fixture(unicode_home)] home: &Path) {
    let (iso, result) = read(home, "single-image.iso", None);
    let [BootPatch { offset, bytes }] = result.unwrap().boot_patches.try_into().unwrap();
    assert_eq!(bytes, image(2, 8192), "the bytes are efisys_noprompt.bin's");

    // Apply the patch to a copy of the ISO, as an overlay shows it: the boot image the firmware loads is now the
    // no-prompt one, and every other byte is the ISO's.
    let original = std::fs::read(&iso).unwrap();
    let mut patched = original.clone();
    patched[offset as usize..offset as usize + bytes.len()].copy_from_slice(&bytes);
    let patched_file = home.join("patched.img");
    std::fs::write(&patched_file, &patched).unwrap();
    assert_eq!(read_at(&patched_file, offset, 8192), image(2, 8192));
    assert_eq!(
        read_at(&iso, offset, 8192),
        image(1, 8192),
        "the ISO held the prompting image there"
    );
    assert_eq!(offset % 2048, 0);
}

#[skuld::test]
fn an_iso_whose_two_boot_images_differ_in_size_is_refused_naming_both_sizes(#[fixture(unicode_home)] home: &Path) {
    let error = read(home, "boot-images-differ-in-size.iso", None).1.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("8192 bytes"), "{error}");
    assert!(error.message().contains("10240 bytes"), "{error}");
    assert!(error.message().contains("efisys_noprompt.bin"), "{error}");
}

#[skuld::test]
fn an_iso_without_a_no_prompt_boot_image_is_refused_naming_the_file(#[fixture(unicode_home)] home: &Path) {
    let error = read(home, "no-no-prompt-image.iso", None).1.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(
        error.message().contains("efi/microsoft/boot/efisys_noprompt.bin"),
        "{error}"
    );
}

#[skuld::test]
fn a_boot_image_that_ends_in_a_hole_is_refused_naming_it(#[fixture(unicode_home)] home: &Path) {
    let error = read(home, "efisys-ending-in-a-hole.iso", None).1.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("efi/microsoft/boot/efisys.bin"), "{error}");
    assert!(error.message().contains("no storage (a gap)"), "{error}");
}

#[skuld::test]
fn a_uefi_boot_image_that_is_not_efisys_bin_is_refused_naming_the_block(#[fixture(unicode_home)] home: &Path) {
    let offset = read(home, "single-image.iso", None).1.unwrap().boot_patches[0].offset;
    let error = read_after(home, "single-image.iso", None, |iso| {
        let mut bytes = std::fs::read(iso).unwrap();
        bytes[offset as usize..offset as usize + 8192].fill(0xaa);
        std::fs::write(iso, bytes).unwrap();
    })
    .1
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("is not efisys.bin"), "{error}");
    assert!(error.message().contains(&format!("block {}", offset / 2048)), "{error}");
    assert!(error.message().contains("efisys_noprompt.bin"), "{error}");
}

#[skuld::test]
fn an_iso_is_keyed_by_its_hash_and_the_image_an_edition_selects(#[fixture(unicode_home)] home: &Path) {
    let iso = copy_of(home, "three-images.iso", |_| {});
    let hash = Sha256::from_bytes(Sha2::digest(std::fs::read(&iso).unwrap()).into());
    let root = key(home, &iso, Some("Windows 11 IoT Enterprise LTSC 2024")).unwrap();
    assert_eq!(
        root.sha256, hash,
        "the hash is of the whole file, whatever was read before it"
    );
    assert_eq!(
        (root.image.index, root.image.name.as_str()),
        (2, "Windows 11 IoT Enterprise LTSC 2024")
    );
    let error = key(home, &iso, None).unwrap_err();
    assert!(error.message().contains("several editions"), "{error}");
}

#[skuld::test]
fn a_root_keyed_from_the_memo_prepares_the_same_boot_patch_as_one_keyed_by_reading_the_iso(
    #[fixture(unicode_home)] home: &Path,
) {
    let iso = copy_of(home, "single-image.iso", |_| {});
    let memo = IsoMemo::new(&Dirs::under(home));
    let progress = MemoryProgress::default();
    let patch = |root: IsoRoot| read_install_iso(root).unwrap().boot_patches;
    let read = patch(key_iso(&TestHost, &memo, &iso, None, None, &progress).unwrap());
    let remembered = patch(key_iso(&TestHost, &memo, &iso, None, None, &progress).unwrap());
    assert_eq!(read, remembered);
    assert_eq!(
        progress
            .messages()
            .iter()
            .filter(|message| message.starts_with("hashing ") && message.contains(": 0 of "))
            .count(),
        1
    );
}

#[skuld::test]
fn a_file_that_is_not_an_iso_is_refused_before_anything_is_made(#[fixture(unicode_home)] home: &Path) {
    let file = home.join("plain.iso");
    std::fs::write(&file, vec![5u8; 700_000]).unwrap();
    let before: Vec<_> = std::fs::read_dir(home)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();

    let error = key(home, &file, None).unwrap_err();
    assert!(
        error.message().contains("plain.iso is not a Windows installer ISO"),
        "{error}"
    );
    let after: Vec<_> = std::fs::read_dir(home)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(before, after);
}

#[skuld::test]
fn a_path_with_space_cyrillic_and_a_surrogate_pair_is_prepared(#[fixture(unicode_home)] home: &Path) {
    let (iso, result) = read(home, "single-image.iso", None);
    assert!(iso.to_string_lossy().contains('\u{1f600}'));
    assert_eq!(result.unwrap().image.name, "Windows 11 Enterprise Evaluation");
}

// install.esd =========================================================================================================

#[skuld::test]
fn an_iso_with_install_esd_lists_its_images_like_one_with_install_wim(#[fixture(unicode_home)] home: &Path) {
    let wim = read(home, "single-image.iso", None).1.unwrap().image;
    let esd = read(home, "install-esd.iso", None).1.unwrap().image;
    assert_eq!(wim, esd);
}

#[skuld::test]
fn an_image_list_in_a_compressed_xml_resource_is_refused_naming_the_file_of_the_iso(
    #[fixture(unicode_home)] home: &Path,
) {
    let error = read(home, "install-esd-compressed-xml.iso", None).1.unwrap_err();
    assert!(error.message().contains("install.esd"), "{error}");
    assert!(error.message().contains("compressed"), "{error}");
}

#[skuld::test]
fn an_iso_with_neither_install_wim_nor_install_esd_is_refused_naming_both(#[fixture(unicode_home)] home: &Path) {
    let error = read(home, "no-install-image.iso", None).1.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("sources/install.wim"), "{error}");
    assert!(error.message().contains("sources/install.esd"), "{error}");
}

// Editions ============================================================================================================

#[skuld::test]
fn a_multi_image_iso_without_edition_is_refused_listing_the_names(#[fixture(unicode_home)] home: &Path) {
    let error = read(home, "three-images.iso", None).1.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    for name in [
        "Windows 11 Enterprise LTSC 2024",
        "Windows 11 IoT Enterprise LTSC 2024",
        "Windows 11 IoT Enterprise Subscription LTSC 2024",
    ] {
        assert!(error.message().contains(&format!("`{name}`")), "{error}");
    }
    assert!(error.message().contains("edition:"), "{error}");
}

#[skuld::test]
fn an_edition_selects_its_image_from_the_iso(#[fixture(unicode_home)] home: &Path) {
    let image = read(home, "three-images.iso", Some("Windows 11 IoT Enterprise LTSC 2024"))
        .1
        .unwrap()
        .image;
    assert_eq!((image.index, image.edition_id.as_str()), (2, "IoTEnterpriseS"));
}

#[skuld::test]
fn an_unknown_edition_lists_the_names(#[fixture(unicode_home)] home: &Path) {
    let error = read(home, "three-images.iso", Some("Windows 11 Home")).1.unwrap_err();
    assert!(error.message().contains("no edition `Windows 11 Home`"), "{error}");
    assert!(error.message().contains("`Windows 11 Enterprise LTSC 2024`"), "{error}");
    // A single-image ISO takes no other name than its own either.
    let error = read(home, "single-image.iso", Some("Windows 11 Home")).1.unwrap_err();
    assert!(
        error.message().contains("`Windows 11 Enterprise Evaluation`"),
        "{error}"
    );
}

#[skuld::test]
fn a_single_image_iso_needs_no_edition(#[fixture(unicode_home)] home: &Path) {
    assert!(read(home, "single-image.iso", None).1.is_ok());
    assert!(
        read(home, "single-image.iso", Some("Windows 11 Enterprise Evaluation"))
            .1
            .is_ok()
    );
}

#[skuld::test]
fn two_images_with_one_name_are_refused_listing_the_indexes() {
    let images = [
        image_of(1, "Same", WimArch::X64),
        image_of(2, "Other", WimArch::X64),
        image_of(3, "Same", WimArch::X64),
    ];
    let error = select_image(&images, Some("Same")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("images 1, 3"), "{error}");
    assert_eq!(select_image(&images, Some("Other")).unwrap().index, 2);
}

#[skuld::test]
fn an_arm64_image_is_refused_naming_arch_and_the_x64_scope() {
    let images = [image_of(1, "Arm", WimArch::Arm64), image_of(2, "Intel", WimArch::X64)];
    let error = select_image(&images, Some("Arm")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotSupported);
    assert!(error.message().contains("arm64 (ARCH 12)"), "{error}");
    assert!(error.message().contains("x64 Windows guests only"), "{error}");
    assert_eq!(select_image(&images, Some("Intel")).unwrap().index, 2);
    let error = select_image(&images[..1], None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotSupported);
}

// The Windows ISO CI builds from ======================================================================================

/// The ISO the `windows_guest` group builds from has the structure the install patch relies on (found on the evaluation
/// ISOs, and measured on an IoT Enterprise LTSC 2024 ISO, which is not one): the two boot images are the same size, and
/// the patch puts the no-prompt one where the boot catalog's UEFI entry loads its image. Nothing here is the address of
/// one ISO build.
#[skuld::test]
fn the_evaluation_iso_boot_patch_has_the_structure_the_spike_found(#[fixture(windows_guest)] _group: &Group) {
    let iso = PathBuf::from(std::env::var_os(WINDOWS_ISO_VAR).expect("the group needs the ISO variable"));
    let held = HeldIso::open(&TestHost, &iso).unwrap();
    let udf = mount(&held).unwrap();
    let boot_image = |path| {
        udf.pieces(path)
            .unwrap()
            .unwrap_or_else(|| panic!("the ISO has no {path}"))
    };
    let (prompting, no_prompt) = (boot_image(PROMPTING_IMAGE), boot_image(NO_PROMPT_IMAGE));
    assert_eq!(
        total(&prompting),
        total(&no_prompt),
        "both boot images are the same size"
    );

    let patch = no_prompt_patch(&udf, &iso).unwrap();

    let entries = udf.boot_entries().unwrap();
    let entry = efi_entry(&entries).expect("a bootable UEFI entry in the boot catalog");
    assert_eq!(
        patch.offset,
        u64::from(entry.load_block) * SECTOR,
        "the patch is where the catalog loads from"
    );
    assert_eq!(
        patch.bytes,
        udf.read_pieces(&no_prompt).unwrap(),
        "the patch is the no-prompt image's bytes"
    );
    assert_eq!(patch.bytes.len() as u64 % SECTOR, 0);
    let size = std::fs::metadata(&iso).unwrap().len();
    assert!(
        patch.offset + patch.bytes.len() as u64 <= size,
        "the patch lies inside the ISO"
    );
}
