use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::{Platform, SECTOR, Udf};
use crate::ErrorKind;
use crate::test_support::fixtures::unicode_home;

pub(crate) fn fixture_iso(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/iso/fixtures")
        .join(name)
}

/// The bytes `make_fixtures.py`'s `image(seed, size)` writes.
pub(crate) fn image(seed: usize, size: usize) -> Vec<u8> {
    (0..size).map(|i| ((seed + i * 7) % 251) as u8).collect()
}

pub(crate) fn read_at(iso: &Path, offset: u64, len: u64) -> Vec<u8> {
    let mut file = std::fs::File::open(iso).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut bytes = vec![0u8; len as usize];
    file.read_exact(&mut bytes).unwrap();
    bytes
}

#[skuld::test]
fn a_file_of_the_udf_tree_is_found_where_its_bytes_are() {
    let iso = fixture_iso("three-images.iso");
    let udf = Udf::open(&iso).unwrap();
    let pieces = udf.pieces("/efi/microsoft/boot/efisys.bin").unwrap().unwrap();
    assert_eq!(pieces.len(), 1);
    assert_eq!(pieces[0].len, 8192);
    assert_eq!(read_at(&iso, pieces[0].offset, pieces[0].len), image(1, 8192));
    let noprompt = udf.pieces("/efi/microsoft/boot/efisys_noprompt.bin").unwrap().unwrap();
    assert_eq!(read_at(&iso, noprompt[0].offset, noprompt[0].len), image(2, 8192));
}

#[skuld::test]
fn a_path_the_tree_does_not_have_is_none() {
    let udf_iso = fixture_iso("three-images.iso");
    let udf = Udf::open(&udf_iso).unwrap();
    assert_eq!(udf.pieces("/sources/install.esd").unwrap(), None);
    assert!(udf.open_file("/sources/install.esd").unwrap().is_none());
}

#[skuld::test]
fn a_file_is_read_through_its_handle() {
    let iso = fixture_iso("three-images.iso");
    let udf = Udf::open(&iso).unwrap();
    let mut file = udf.open_file("/sources/install.wim").unwrap().unwrap();
    let mut tag = [0u8; 8];
    file.read_exact(&mut tag).unwrap();
    assert_eq!(&tag, b"MSWIM\0\0\0");
    assert!(file.seek(hadris_fs::SeekFrom::End(0)).unwrap() > 8000);
}

#[skuld::test]
fn the_boot_catalog_lists_the_bios_and_the_efi_image_that_the_firmware_loads() {
    let iso = fixture_iso("three-images.iso");
    let entries = Udf::open(&iso).unwrap().boot_entries().unwrap();
    let platforms: Vec<Platform> = entries.iter().map(|entry| entry.platform).collect();
    assert_eq!(platforms, [Platform::Bios, Platform::Efi]);
    assert!(entries.iter().all(|entry| entry.bootable));
    let efi = entries[1];
    // The boot image is a copy of efisys.bin kept apart from the UDF file.
    assert_eq!(read_at(&iso, u64::from(efi.load_block) * SECTOR, 8192), image(1, 8192));
}

#[skuld::test]
fn a_file_that_is_not_an_iso_is_refused_naming_it(#[fixture(unicode_home)] home: &Path) {
    let file = home.join("not an iso.iso");
    std::fs::write(&file, vec![9u8; 600_000]).unwrap();
    let error = Udf::open(&file).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("not an iso.iso"), "{error}");
    assert!(error.message().contains("no UDF file system"), "{error}");
}
