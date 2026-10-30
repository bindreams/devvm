use std::io::{Cursor, Read};
use std::path::Path;

use fatfs::{FileSystem, FsOptions};

use super::{SeedFile, write_cidata_image};
use crate::backend::MediaKind;
use crate::test_support::fixtures::unicode_home;

fn file(name: &str, bytes: &[u8]) -> SeedFile {
    SeedFile {
        name: name.to_owned(),
        bytes: bytes.to_vec(),
    }
}

/// Every byte value, so a text-mode or UTF-8 conversion anywhere would show.
fn all_bytes() -> Vec<u8> {
    (0..=255u8).cycle().take(3000).collect()
}

fn open(image: &Path) -> FileSystem<Cursor<Vec<u8>>> {
    let bytes = std::fs::read(image).unwrap();
    FileSystem::new(Cursor::new(bytes), FsOptions::new()).unwrap()
}

/// The contents of the file `name` in the seed image at `image`.
pub(crate) fn read_image_file(image: &Path, name: &str) -> Vec<u8> {
    read_file(&open(image), name)
}

fn read_file(fs: &FileSystem<Cursor<Vec<u8>>>, name: &str) -> Vec<u8> {
    let mut content = Vec::new();
    fs.root_dir()
        .open_file(name)
        .unwrap()
        .read_to_end(&mut content)
        .unwrap();
    content
}

#[skuld::test]
fn image_has_label_cidata_and_both_files(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("seed.img");
    write_cidata_image(
        &[
            file("user-data", b"#cloud-config\n"),
            file("meta-data", b"instance-id: x\n"),
        ],
        &image,
    )
    .unwrap();

    let fs = open(&image);
    assert_eq!(fs.volume_label(), "CIDATA");
    let mut names: Vec<String> = fs.root_dir().iter().map(|entry| entry.unwrap().file_name()).collect();
    names.sort();
    assert_eq!(names, ["meta-data", "user-data"]);
}

#[skuld::test]
fn byte_exact_contents(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("seed.img");
    let binary = all_bytes();
    let text = "hostname: \u{e9}\u{416}\u{4e16}\u{1f600}\r\nline\n".as_bytes();
    write_cidata_image(
        &[file("user-data", &binary), file("meta-data", text), file("empty", b"")],
        &image,
    )
    .unwrap();

    let fs = open(&image);
    assert_eq!(read_file(&fs, "user-data"), binary);
    assert_eq!(read_file(&fs, "meta-data"), text);
    assert_eq!(read_file(&fs, "empty"), b"");
}

#[skuld::test]
fn a_large_file_grows_the_image_instead_of_failing(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("seed.img");
    let big: Vec<u8> = (0..5_000_000u32).map(|i| (i % 251) as u8).collect();
    write_cidata_image(&[file("user-data", &big), file("meta-data", b"m")], &image).unwrap();

    let fs = open(&image);
    assert_eq!(read_file(&fs, "user-data"), big);
    assert_eq!(fs.volume_label(), "CIDATA");
}

#[skuld::test]
fn lands_under_a_non_ascii_directory(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("seed.img");
    let media = write_cidata_image(&[file("user-data", b"u"), file("meta-data", b"m")], &image).unwrap();

    assert_eq!(media.path, image);
    assert!(image.is_file());
    assert!(
        image
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains('\u{1f600}')
    );
}

#[skuld::test]
fn a_seed_is_deterministic_for_equal_inputs(#[fixture(unicode_home)] home: &Path) {
    let files = [file("user-data", &all_bytes()), file("meta-data", b"instance-id: x\n")];
    let (first, second) = (home.join("one.img"), home.join("two.img"));
    write_cidata_image(&files, &first).unwrap();
    write_cidata_image(&files, &second).unwrap();
    assert_eq!(std::fs::read(&first).unwrap(), std::fs::read(&second).unwrap());

    let other = [file("user-data", b"different"), file("meta-data", b"instance-id: x\n")];
    let third = home.join("three.img");
    write_cidata_image(&other, &third).unwrap();
    assert_ne!(std::fs::read(&first).unwrap(), std::fs::read(&third).unwrap());
}

#[skuld::test]
fn the_returned_media_is_readonly_and_its_kind_matches_the_format(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("seed.img");
    let media = write_cidata_image(&[file("user-data", b"u"), file("meta-data", b"m")], &image).unwrap();

    assert_eq!(media.kind, MediaKind::Disk, "a FAT volume is attached as a disk");
    assert!(media.readonly);
    assert_eq!(media.boot_index, None);
    assert_eq!(fatfs_type(&image), fatfs::FatType::Fat12);
}

fn fatfs_type(image: &Path) -> fatfs::FatType {
    open(image).fat_type()
}

#[skuld::test]
fn an_unwritable_destination_is_an_io_error(#[fixture(unicode_home)] home: &Path) {
    let error = write_cidata_image(&[file("user-data", b"u")], &home.join("missing").join("seed.img"))
        .expect_err("the directory does not exist");
    assert_eq!(error.kind(), crate::ErrorKind::Io);
    assert!(error.message().contains("seed.img"), "{}", error.message());
}
