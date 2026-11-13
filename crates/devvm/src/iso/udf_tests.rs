use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use hadris_fs::Extent;

use super::{MapError, PAGE, Piece, Platform, SECTOR, Udf, map_extents};
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
fn a_file_in_more_extents_than_a_page_holds_is_found_whole() {
    let iso = iso_fixture("install-wim-in-many-extents.iso");
    let udf = open(&iso).unwrap();
    let pieces = udf.pieces("/sources/install.wim").unwrap().unwrap();
    assert_eq!(pieces.len(), 20, "{pieces:?}");
    for pair in pieces.windows(2) {
        assert_eq!(pair[0].offset + pair[0].len, pair[1].offset, "{pieces:?}");
    }
    let mut file = udf.open_file("/sources/install.wim").unwrap().unwrap();
    let length = file.seek(hadris_fs::SeekFrom::End(0)).unwrap();
    assert_eq!(pieces.iter().map(|piece| piece.len).sum::<u64>(), length);
    assert_eq!(udf.read_pieces(&pieces).unwrap().len() as u64, length);
}

#[skuld::test]
fn an_empty_file_has_no_pieces() {
    let iso = iso_fixture("empty-install-wim.iso");
    assert_eq!(
        open(&iso).unwrap().pieces("/sources/install.wim").unwrap(),
        Some(vec![])
    );
}

#[skuld::test]
fn what_is_not_a_whole_file_in_the_iso_is_refused_naming_it() {
    for (fixture, path, why) in [
        (
            "install-wim-with-an-unwritten-extent.iso",
            "/sources/install.wim",
            "never written",
        ),
        (
            "install-wim-with-a-hole.iso",
            "/sources/install.wim",
            "no storage (a gap)",
        ),
        (
            "install-wim-ending-in-a-hole.iso",
            "/sources/install.wim",
            "no storage (a gap)",
        ),
        (
            "install-wim-starting-with-a-hole.iso",
            "/sources/install.wim",
            "no storage (a gap)",
        ),
        (
            "install-wim-with-a-hole-after-a-full-page.iso",
            "/sources/install.wim",
            "no storage (a gap)",
        ),
        (
            "install-wim-that-is-a-pipe.iso",
            "/sources/install.wim",
            "is not a file",
        ),
        (
            "install-wim-that-is-a-link.iso",
            "/sources/install.wim",
            "is a link, not a file",
        ),
        ("three-images.iso", "/sources", "is a directory, not a file"),
    ] {
        let iso = iso_fixture(fixture);
        let error = open(&iso).unwrap().pieces(path).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Recipe, "{fixture}: {error}");
        assert!(error.message().contains(fixture), "{fixture}: {error}");
        assert!(
            error.message().contains(path.trim_start_matches('/')),
            "{fixture}: {error}"
        );
        assert!(error.message().contains(why), "{fixture}: {error}");
    }
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

// The extents of a file, as `extents` pages them, made up -------------------------------------------------------------

/// The pages `extents` would return, one after the other, whatever `from` is asked for.
fn pages_of(mut pages: Vec<Vec<Extent>>) -> impl FnMut(u64, &mut [Extent]) -> std::result::Result<usize, ()> {
    pages.reverse();
    move |_, out| {
        let page = pages.pop().expect("asked for a page after the last one");
        out[..page.len()].copy_from_slice(&page);
        Ok(page.len())
    }
}

fn extent(offset: u64, file_offset: u64, len: u64) -> Extent {
    Extent::new(offset, len).with_file_offset(file_offset)
}

fn refusal(length: u64, pages: Vec<Vec<Extent>>) -> &'static str {
    match map_extents(length, pages_of(pages)) {
        Err(MapError::Refused(why)) => why,
        other => panic!("not refused: {other:?}"),
    }
}

#[skuld::test]
fn extents_that_follow_one_another_to_the_length_are_pieces() {
    let pieces = map_extents(
        4096,
        pages_of(vec![vec![extent(10_000, 0, 2048), extent(50_000, 2048, 2048)]]),
    )
    .unwrap();
    assert_eq!(
        pieces,
        [
            Piece {
                offset: 10_000,
                len: 2048
            },
            Piece {
                offset: 50_000,
                len: 2048
            }
        ]
    );
}

#[skuld::test]
fn a_file_of_no_length_has_no_pieces() {
    assert_eq!(map_extents(0, pages_of(vec![vec![]])).unwrap(), []);
}

#[skuld::test]
fn an_extent_that_starts_after_the_end_of_the_one_before_is_a_gap() {
    assert!(refusal(6144, vec![vec![extent(0, 0, 2048), extent(9000, 4096, 2048)]]).contains("a gap"));
    assert!(refusal(4096, vec![vec![extent(9000, 2048, 2048)]]).contains("a gap"));
}

#[skuld::test]
fn extents_that_end_before_the_length_are_a_gap() {
    assert!(refusal(4096, vec![vec![extent(0, 0, 2048)]]).contains("a gap"));
}

#[skuld::test]
fn an_extent_that_starts_before_the_end_of_the_one_before_overlaps_it() {
    assert!(refusal(3072, vec![vec![extent(0, 0, 2048), extent(9000, 1024, 2048)]]).contains("overlap"));
}

#[skuld::test]
fn an_extent_that_runs_past_the_length_is_refused() {
    assert!(refusal(2048, vec![vec![extent(0, 0, 4096)]]).contains("past its end"));
}

#[skuld::test]
fn extents_whose_ends_overflow_are_refused_not_wrapped() {
    let refused = refusal(u64::MAX, vec![vec![extent(0, 0, u64::MAX), extent(9000, u64::MAX, 2)]]);
    assert!(refused.contains("out of range"), "{refused}");
}

#[skuld::test]
fn a_part_that_was_never_written_is_refused() {
    let refused = refusal(2048, vec![vec![extent(0, 0, 2048).with_unwritten()]]);
    assert!(refused.contains("never written"), "{refused}");
}

#[skuld::test]
fn a_full_page_is_followed_by_the_next_until_one_is_not_full() {
    let first: Vec<Extent> = (0..PAGE as u64).map(|i| extent(1000 * i, 2048 * i, 2048)).collect();
    let second = vec![extent(7, 2048 * PAGE as u64, 2048)];
    let pieces = map_extents(2048 * (PAGE as u64 + 1), pages_of(vec![first, second])).unwrap();
    assert_eq!(pieces.len(), PAGE + 1);
}

#[skuld::test]
fn a_full_page_after_which_the_listing_does_not_advance_is_an_error_not_a_hang() {
    // Empty parts fill a page and leave the listing where it began.
    let page = vec![extent(0, 0, 0); PAGE];
    let mut asked = 0;
    let outcome = map_extents(0, |_, out: &mut [Extent]| -> std::result::Result<usize, ()> {
        asked += 1;
        assert_eq!(asked, 1, "asked for another page of a listing that did not advance");
        out[..PAGE].copy_from_slice(&page);
        Ok(PAGE)
    });
    match outcome {
        Err(MapError::Refused(why)) => assert!(why.contains("does not advance"), "{why}"),
        other => panic!("not refused: {other:?}"),
    }
}

#[skuld::test]
fn a_failed_page_is_passed_on() {
    let outcome = map_extents(2048, |_, _: &mut [Extent]| Err("boom"));
    assert!(matches!(outcome, Err(MapError::Page("boom"))), "{outcome:?}");
}
