use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::{Platform, SECTOR, Udf};
use crate::test_support::fixtures::{iso_fixture, unicode_home};
use crate::{ErrorKind, Result};

fn open(iso: &Path) -> Result<Udf<'_>> {
    Udf::open(iso, std::fs::File::open(iso).unwrap())
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
    let iso = iso_fixture("three-images.iso");
    let udf = open(&iso).unwrap();
    let pieces = udf.pieces("/efi/microsoft/boot/efisys.bin").unwrap().unwrap();
    assert_eq!(pieces.len(), 1);
    assert_eq!(pieces[0].len, 8192);
    assert_eq!(read_at(&iso, pieces[0].offset, pieces[0].len), image(1, 8192));
    let noprompt = udf.pieces("/efi/microsoft/boot/efisys_noprompt.bin").unwrap().unwrap();
    assert_eq!(read_at(&iso, noprompt[0].offset, noprompt[0].len), image(2, 8192));
}

#[skuld::test]
fn a_path_the_tree_does_not_have_is_none() {
    let udf_iso = iso_fixture("three-images.iso");
    let udf = open(&udf_iso).unwrap();
    assert_eq!(udf.pieces("/sources/install.esd").unwrap(), None);
    assert!(udf.open_file("/sources/install.esd").unwrap().is_none());
}

#[skuld::test]
fn a_file_is_read_through_its_handle() {
    let iso = iso_fixture("three-images.iso");
    let udf = open(&iso).unwrap();
    let mut file = udf.open_file("/sources/install.wim").unwrap().unwrap();
    let mut tag = [0u8; 8];
    file.read_exact(&mut tag).unwrap();
    assert_eq!(&tag, b"MSWIM\0\0\0");
    assert!(file.seek(hadris_fs::SeekFrom::End(0)).unwrap() > 8000);
}

#[skuld::test]
fn the_boot_catalog_lists_the_bios_and_the_efi_image_that_the_firmware_loads() {
    let iso = iso_fixture("three-images.iso");
    let entries = open(&iso).unwrap().boot_entries().unwrap();
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
    let error = open(&file).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("not an iso.iso"), "{error}");
    assert!(error.message().contains("no UDF file system"), "{error}");
}

#[skuld::test]
fn the_tree_is_read_through_the_handle_it_was_opened_with_not_the_path(#[fixture(unicode_home)] home: &Path) {
    let iso = home.join("kept.iso");
    std::fs::copy(iso_fixture("three-images.iso"), &iso).unwrap();
    let udf = open(&iso).unwrap();
    // Another file takes the ISO's path.
    std::fs::write(home.join("other.iso"), vec![9u8; 700_000]).unwrap();
    std::fs::rename(home.join("other.iso"), &iso).unwrap();

    let pieces = udf.pieces("/efi/microsoft/boot/efisys.bin").unwrap().unwrap();
    assert_eq!(udf.read_pieces(&pieces).unwrap(), image(1, 8192));
    assert_eq!(udf.boot_entries().unwrap().len(), 2);
    assert!(udf.open_file("/sources/install.wim").unwrap().is_some());
}

#[skuld::test]
fn bytes_that_lie_past_the_end_of_the_iso_are_refused_naming_it() {
    let iso = iso_fixture("three-images.iso");
    let udf = open(&iso).unwrap();
    let past = udf
        .read_pieces(&[super::Piece {
            offset: 1 << 40,
            len: 8,
        }])
        .unwrap_err();
    assert_eq!(past.kind(), ErrorKind::Recipe);
    assert!(past.message().contains("three-images.iso"), "{past}");
    assert!(past.message().contains("outside the file"), "{past}");
}

#[skuld::test]
fn a_file_in_several_extents_is_found_piece_by_piece_and_read_across_them() {
    let iso = iso_fixture("install-wim-in-two-extents.iso");
    let udf = open(&iso).unwrap();
    let pieces = udf.pieces("/sources/install.wim").unwrap().unwrap();
    assert!(pieces.len() > 1, "{pieces:?}");
    assert_eq!(pieces[0].offset + pieces[0].len, pieces[1].offset);
    // The image list lies in the second extent.
    let mut file = udf.open_file("/sources/install.wim").unwrap().unwrap();
    let list = crate::iso::wim::read_image_list(&mut file, "install.wim").unwrap();
    assert_eq!(list.len(), 1, "{list:?}");
}

#[skuld::test]
fn a_file_with_an_extent_that_was_never_written_is_refused_naming_it() {
    let iso = iso_fixture("install-wim-with-an-unwritten-extent.iso");
    let error = open(&iso).unwrap().pieces("/sources/install.wim").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(
        error.message().contains("install-wim-with-an-unwritten-extent.iso"),
        "{error}"
    );
    assert!(error.message().contains("sources/install.wim"), "{error}");
    assert!(error.message().contains("never written"), "{error}");
}

#[skuld::test]
fn a_file_with_a_gap_between_its_extents_is_refused_naming_it() {
    let iso = iso_fixture("install-wim-with-a-hole.iso");
    let error = open(&iso).unwrap().pieces("/sources/install.wim").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("sources/install.wim"), "{error}");
    assert!(error.message().contains("gap"), "{error}");
}

#[skuld::test]
fn a_file_that_ends_in_a_hole_is_refused_naming_it() {
    let iso = iso_fixture("install-wim-ending-in-a-hole.iso");
    let error = open(&iso).unwrap().pieces("/sources/install.wim").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("install-wim-ending-in-a-hole.iso"), "{error}");
    assert!(error.message().contains("sources/install.wim"), "{error}");
    assert!(error.message().contains("no storage"), "{error}");
}

#[skuld::test]
fn names_are_found_whatever_their_case() {
    let iso = iso_fixture("upper-case-names.iso");
    let udf = open(&iso).unwrap();
    assert!(udf.pieces("/efi/microsoft/boot/efisys.bin").unwrap().is_some());
    assert!(udf.pieces("/efi/microsoft/boot/efisys_noprompt.bin").unwrap().is_some());
    assert!(udf.open_file("/sources/install.wim").unwrap().is_some());
    assert_eq!(udf.pieces("/sources/install.esd").unwrap(), None);
    assert_eq!(udf.pieces("/sources/install.wim/inside").unwrap(), None);
}

#[skuld::test]
fn two_names_that_differ_only_in_case_are_refused_naming_both() {
    let iso = iso_fixture("two-install-names-of-one-spelling.iso");
    let udf = open(&iso).unwrap();
    let errors = [
        udf.pieces("/sources/install.wim").unwrap_err(),
        udf.open_file("/sources/install.wim").err().unwrap(),
    ];
    for error in errors {
        assert_eq!(error.kind(), ErrorKind::Recipe);
        assert!(
            error.message().contains("install.wim") && error.message().contains("INSTALL.WIM"),
            "{error}"
        );
    }
}
