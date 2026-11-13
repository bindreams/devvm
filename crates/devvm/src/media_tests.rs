use std::io::{Cursor, Read, Write};
use std::path::Path;

use fatfs::{FileSystem, FsOptions};

use super::{
    LARGEST_FAT12_CONTENTS, SeedFile, fat_image, format_options, image_size, read_files, unchecked_image_size,
    write_cidata_image,
};
use crate::backend::MediaKind;
use crate::host::symlink_metadata_if_present;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::host::{RecordingHost, TestHost};

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
        &TestHost,
        &[
            file("user-data", b"#cloud-config\n"),
            file("meta-data", b"instance-id: x\n"),
        ],
        MediaKind::Disk,
        true,
        0,
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
        &TestHost,
        &[file("user-data", &binary), file("meta-data", text), file("empty", b"")],
        MediaKind::Disk,
        true,
        0,
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
    write_cidata_image(
        &TestHost,
        &[file("user-data", &big), file("meta-data", b"m")],
        MediaKind::Disk,
        true,
        0,
        &image,
    )
    .unwrap();

    let fs = open(&image);
    assert_eq!(read_file(&fs, "user-data"), big);
    assert_eq!(fs.volume_label(), "CIDATA");
}

#[skuld::test]
fn lands_under_a_non_ascii_directory(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("seed.img");
    let media = write_cidata_image(
        &TestHost,
        &[file("user-data", b"u"), file("meta-data", b"m")],
        MediaKind::Disk,
        true,
        0,
        &image,
    )
    .unwrap();

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
    write_cidata_image(&TestHost, &files, MediaKind::Disk, true, 0, &first).unwrap();
    write_cidata_image(&TestHost, &files, MediaKind::Disk, true, 0, &second).unwrap();
    assert_eq!(std::fs::read(&first).unwrap(), std::fs::read(&second).unwrap());

    let other = [file("user-data", b"different"), file("meta-data", b"instance-id: x\n")];
    let third = home.join("three.img");
    write_cidata_image(&TestHost, &other, MediaKind::Disk, true, 0, &third).unwrap();
    assert_ne!(std::fs::read(&first).unwrap(), std::fs::read(&third).unwrap());
}

#[skuld::test]
fn the_returned_media_is_readonly_and_its_kind_matches_the_format(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("seed.img");
    let media = write_cidata_image(
        &TestHost,
        &[file("user-data", b"u"), file("meta-data", b"m")],
        MediaKind::Disk,
        true,
        0,
        &image,
    )
    .unwrap();

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
    let error = write_cidata_image(
        &TestHost,
        &[file("user-data", b"u")],
        MediaKind::Disk,
        true,
        0,
        &home.join("missing").join("seed.img"),
    )
    .expect_err("the directory does not exist");
    assert_eq!(error.kind(), crate::ErrorKind::Io);
    assert!(error.message().contains("seed.img"), "{}", error.message());
}

#[skuld::test]
fn a_repeated_name_is_refused_whatever_its_case(#[fixture(unicode_home)] home: &Path) {
    // FAT looks names up case-insensitively, so `USER-DATA` and `user-data` are one directory entry.
    for (first, second) in [
        ("user-data", "user-data"),
        ("user-data", "USER-DATA"),
        ("\u{e9}", "\u{c9}"),
    ] {
        let image = home.join("seed.img");
        let error = write_cidata_image(
            &TestHost,
            &[file(first, b"one"), file("meta-data", b"m"), file(second, b"two")],
            MediaKind::Disk,
            true,
            0,
            &image,
        )
        .expect_err("two files cannot share a FAT name");
        assert_eq!(error.kind(), crate::ErrorKind::Internal, "{first} {second}");
        assert!(
            error.message().contains(first) && error.message().contains(second),
            "both names are reported: {}",
            error.message()
        );
        assert!(!image.exists(), "nothing is written for a refused set");
    }
}

#[skuld::test]
fn a_name_fatfs_would_split_or_trim_is_refused(#[fixture(unicode_home)] home: &Path) {
    // `fatfs` trims `/` and walks the rest as a path, so `user-data/` is `user-data` and `a/b` needs a directory.
    for bad in ["", ".", "..", "/", "user-data/", "/user-data", "a/b", "a\\b"] {
        let image = home.join("seed.img");
        let error = write_cidata_image(
            &TestHost,
            &[file("user-data", b"one"), file(bad, b"two")],
            MediaKind::Disk,
            true,
            0,
            &image,
        )
        .expect_err("the name is not a plain FAT file name");
        assert_eq!(error.kind(), crate::ErrorKind::Internal, "{bad:?}");
        assert!(
            error.message().contains(&format!("`{bad}`")),
            "{bad:?}: {}",
            error.message()
        );
        assert!(!image.exists(), "nothing is written for a refused set");
    }
}

#[skuld::test]
fn name_sets_fatfs_would_merge_or_rename_are_refused(#[fixture(unicode_home)] home: &Path) {
    // Each set is written by `fatfs` as fewer files than asked, or under another name: a generated 8.3 short name
    // matches a long name, a trailing dot is dropped, U+FFFF is LFN padding.
    let sets: [&[&str]; 11] = [
        &["user-data", "USER-D~1"],
        &["meta-data", "META-D~1"],
        &["abcdefghij", "ABCDEF~1"],
        &["a b", "AB~1"],
        &["a.b.c", "AB~1.C"],
        &["abcdefghij.txt", "ABCDEF~1.TXT"],
        &["a.", "a"],
        &["a.", "A"],
        &["a", "a\u{ffff}"],
        &["a\u{ffff}", "a"],
        &["\u{ffff}"],
    ];
    for names in sets {
        let image = home.join("seed.img");
        let files: Vec<SeedFile> = names.iter().map(|name| file(name, b"x")).collect();
        let error = write_cidata_image(&TestHost, &files, MediaKind::Disk, true, 0, &image)
            .expect_err("fatfs would not write these names as asked");
        assert_eq!(error.kind(), crate::ErrorKind::Internal, "{names:?}");
        for name in names {
            assert!(
                error.message().contains(&format!("{name:?}")),
                "{names:?}: {name:?} is not named: {}",
                error.message()
            );
        }
        assert!(!image.exists(), "nothing is written for a refused set");
    }
}

#[skuld::test]
fn a_name_fatfs_rejects_is_refused_by_name_with_fatfs_s_reason(#[fixture(unicode_home)] home: &Path) {
    // The reasons are fatfs 0.3.6's fixed messages; `unusable_name` is chosen by matching them, so a fatfs that words
    // them differently fails here instead of blaming the wrong thing.
    let too_long = "a".repeat(256);
    let cases = [
        ("a:b", "File name contains unsupported characters"),
        ("a*", "File name contains unsupported characters"),
        ("a\tb", "File name contains unsupported characters"),
        ("a\0b", "File name contains unsupported characters"),
        ("\u{1f600}", "File name contains unsupported characters"),
        (too_long.as_str(), "File name too long"),
    ];
    for (bad, reason) in cases {
        let image = home.join("seed.img");
        let error = write_cidata_image(
            &TestHost,
            &[file("user-data", b"one"), file(bad, b"two")],
            MediaKind::Disk,
            true,
            0,
            &image,
        )
        .expect_err("fatfs refuses the name");
        assert_eq!(error.kind(), crate::ErrorKind::Internal, "{bad:?}");
        assert!(
            error.message().contains(&format!("`{bad}` cannot be a file name")) && error.message().contains(reason),
            "{bad:?}: {}",
            error.message()
        );
        assert!(!image.exists(), "nothing is written for a refused set");
    }
}

#[skuld::test]
fn a_failure_that_is_not_the_name_does_not_blame_the_name(#[fixture(unicode_home)] home: &Path) {
    // Every name is fine; the root directory of the 1 MiB image runs out of entries, which fatfs reports as "No space
    // left on device". Each of these names takes two of the directory's 512 entries (a long-name entry and the short one).
    let files: Vec<SeedFile> = (0..300).map(|n| file(&format!("file-{n}"), b"")).collect();
    let image = home.join("seed.img");
    let error = write_cidata_image(&TestHost, &files, MediaKind::Disk, true, 0, &image)
        .expect_err("the root directory is full");
    assert_eq!(error.kind(), crate::ErrorKind::Internal);
    assert!(
        error.message().starts_with("cannot build the seed image: "),
        "{}",
        error.message()
    );
    assert!(
        !error.message().contains("cannot be a file name"),
        "a name is blamed: {}",
        error.message()
    );
    assert!(!image.exists(), "nothing is written for a failed build");
}

#[skuld::test]
fn names_fatfs_keeps_apart_are_written_as_asked(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("seed.img");
    let names = [
        "user-data",
        "meta-data",
        "a.b",
        "\u{e9}",
        "\u{4e16}\u{754c}",
        "with space",
        "a.b.c.d",
    ];
    let files: Vec<SeedFile> = names.iter().map(|name| file(name, name.as_bytes())).collect();
    write_cidata_image(&TestHost, &files, MediaKind::Disk, true, 0, &image).unwrap();

    let fs = open(&image);
    for name in names {
        assert_eq!(read_file(&fs, name), name.as_bytes(), "{name}");
    }
}

#[skuld::test]
fn the_image_is_written_as_a_private_file(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("seed.img");
    let host = RecordingHost::new();
    write_cidata_image(
        &host,
        &[file("user-data", b"u"), file("meta-data", b"m")],
        MediaKind::Disk,
        true,
        0,
        &image,
    )
    .unwrap();
    assert_eq!(host.private_writes(), [image]);
}

#[skuld::test]
fn an_image_left_by_an_earlier_launch_is_replaced(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("seed.img");
    write_cidata_image(
        &TestHost,
        &[file("user-data", b"old")],
        MediaKind::Disk,
        true,
        0,
        &image,
    )
    .unwrap();
    write_cidata_image(
        &TestHost,
        &[file("user-data", b"new")],
        MediaKind::Disk,
        true,
        0,
        &image,
    )
    .unwrap();
    assert_eq!(read_image_file(&image, "user-data"), b"new");
}

#[skuld::test]
fn a_refused_private_write_is_an_io_error_naming_the_image(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("seed.img");
    let host = RecordingHost::new();
    host.fail_private_write(0);
    let error = write_cidata_image(&host, &[file("user-data", b"u")], MediaKind::Disk, true, 0, &image)
        .expect_err("the host refused the write");
    assert_eq!(error.kind(), crate::ErrorKind::Io);
    assert!(error.message().contains("seed.img"), "{}", error.message());
    assert!(!image.exists());
}

#[skuld::test]
fn a_seed_beyond_the_fat12_limit_is_refused_and_leaves_an_earlier_image_untouched(
    #[fixture(unicode_home)] home: &Path,
) {
    let image = home.join("seed.img");
    let host = RecordingHost::new();
    let too_large = [file("user-data", &vec![7u8; LARGEST_FAT12_CONTENTS as usize + 1])];

    let error = write_cidata_image(&host, &too_large, MediaKind::Disk, true, 0, &image).expect_err("over the limit");
    assert_eq!(error.kind(), crate::ErrorKind::Internal, "{error}");
    assert!(matches!(symlink_metadata_if_present(&image), Ok(None)));
    assert!(host.private_writes().is_empty());

    std::fs::write(&image, b"left by an earlier launch").unwrap();
    write_cidata_image(&host, &too_large, MediaKind::Disk, true, 0, &image).expect_err("over the limit");
    assert_eq!(std::fs::read(&image).unwrap(), b"left by an earlier launch");
    assert!(host.private_writes().is_empty());
}

fn type_of(image: Vec<u8>) -> fatfs::FatType {
    FileSystem::new(Cursor::new(image), FsOptions::new())
        .unwrap()
        .fat_type()
}

#[skuld::test]
fn the_image_is_fat12_whatever_the_contents() {
    let seed = [file("user-data", &all_bytes()), file("meta-data", b"instance-id: x\n")];
    let large = [file("user-data", &vec![7u8; 5_000_000]), file("meta-data", b"m")];
    let largest = [file("user-data", &vec![7u8; LARGEST_FAT12_CONTENTS as usize])];
    for (what, files) in [
        ("none", &[][..]),
        ("a seed", &seed),
        ("5,000,000 bytes", &large),
        ("the largest", &largest),
    ] {
        let image = fat_image(files, 0).unwrap();
        assert_eq!(type_of(image), fatfs::FatType::Fat12, "{what}");
    }
}

/// The limit is tight: one byte more than [`LARGEST_FAT12_CONTENTS`], at the size the formula gives, does not format as
/// FAT12 (`fatfs` either fails or picks another type).
#[skuld::test]
fn one_byte_beyond_the_fat12_limit_does_not_format_as_fat12() {
    let size = unchecked_image_size(LARGEST_FAT12_CONTENTS + 1) as u64;
    let mut disk = Cursor::new(vec![0u8; size as usize]);
    if fatfs::format_volume(&mut disk, format_options(size)).is_ok() {
        assert_ne!(type_of(disk.into_inner()), fatfs::FatType::Fat12);
    }
}

#[skuld::test]
fn the_image_size_is_refused_beyond_the_fat12_limit_naming_the_sum_and_the_image_size() {
    assert_eq!(image_size(LARGEST_FAT12_CONTENTS, 0).unwrap(), 133_885_952);

    let error = image_size(LARGEST_FAT12_CONTENTS + 1, 0).expect_err("over the limit");
    assert_eq!(error.kind(), crate::ErrorKind::Internal, "{error}");
    assert_eq!(
        error.message(),
        concat!(
            "the seed files (66418689 bytes) need an image of 133886464 bytes, which would not be FAT12: ",
            "at most 66418688 bytes of files and spare room fit"
        )
    );

    for contents in [1 << 40, u64::MAX] {
        let error = image_size(contents, 0).expect_err("over the limit");
        assert_eq!(error.kind(), crate::ErrorKind::Internal, "{error}");
        assert!(error.message().contains(&contents.to_string()), "{error}");
        assert!(error.message().contains("FAT12"), "{error}");
    }
}

// The caller's kind, read-only flag and spare room --------------------------------------------------------------------

#[skuld::test]
fn the_returned_media_has_the_kind_and_the_read_only_flag_the_caller_asked_for(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("answer.img");
    let media = write_cidata_image(&TestHost, &[file("a", b"1")], MediaKind::UsbDisk, false, 0, &image).unwrap();
    assert_eq!(media.kind, MediaKind::UsbDisk);
    assert!(!media.readonly);
    assert_eq!(media.path, image);
    assert_eq!(media.boot_index, None);
}

#[skuld::test]
fn an_image_built_with_spare_room_leaves_it_free(#[fixture(unicode_home)] home: &Path) {
    let spare = 8 << 20;
    let (with, without) = (home.join("with.img"), home.join("without.img"));
    let files = [file("winpe.cmd", b"echo")];
    write_cidata_image(&TestHost, &files, MediaKind::UsbDisk, false, spare, &with).unwrap();
    write_cidata_image(&TestHost, &files, MediaKind::UsbDisk, false, 0, &without).unwrap();

    let free = |image: &Path| {
        let stats = open(image).stats().unwrap();
        u64::from(stats.free_clusters()) * u64::from(stats.cluster_size())
    };
    assert!(free(&with) >= spare, "{} bytes free of {spare} asked", free(&with));
    assert!(free(&without) < spare);
}

#[skuld::test]
fn an_image_with_spare_room_is_still_fat12_and_holds_its_files(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("answer.img");
    write_cidata_image(
        &TestHost,
        &[file("a", b"1")],
        MediaKind::UsbDisk,
        false,
        8 << 20,
        &image,
    )
    .unwrap();
    let fs = open(&image);
    assert_eq!(fs.fat_type(), fatfs::FatType::Fat12);
    assert_eq!(read_file(&fs, "a"), b"1");
}

#[skuld::test]
fn spare_room_counts_toward_the_fat12_limit() {
    let error = fat_image(&[file("a", b"1")], LARGEST_FAT12_CONTENTS).expect_err("over the limit");
    assert_eq!(error.kind(), crate::ErrorKind::Internal, "{error}");
}

#[skuld::test]
fn a_spare_that_overfills_the_volume_is_named_apart_from_the_files() {
    let error = fat_image(&[file("a", &[0; 10])], LARGEST_FAT12_CONTENTS).expect_err("over the limit");

    assert_eq!(
        error.message(),
        concat!(
            "the seed files (10 bytes) and 66418688 bytes of spare room need an image of 133886464 bytes, ",
            "which would not be FAT12: at most 66418688 bytes of files and spare room fit"
        )
    );
}

// read_files ----------------------------------------------------------------------------------------------------------

/// A FAT volume holding `files` at its root and, in directories a guest made, `nested` (path with `/`, bytes).
fn volume_with(files: &[SeedFile], nested: &[(&str, &[u8])]) -> Vec<u8> {
    let mut disk = Cursor::new(fat_image(files, 1 << 20).unwrap());
    {
        let fs = FileSystem::new(&mut disk, FsOptions::new()).unwrap();
        for (path, bytes) in nested {
            let mut dir = fs.root_dir();
            let mut parts: Vec<&str> = path.split('/').collect();
            let name = parts.pop().unwrap();
            for part in parts {
                dir = dir.open_dir(part).or_else(|_| dir.create_dir(part)).unwrap();
            }
            dir.create_file(name).unwrap().write_all(bytes).unwrap();
        }
        fs.unmount().unwrap();
    }
    disk.into_inner()
}

fn names_of(files: &[SeedFile]) -> Vec<&str> {
    let mut names: Vec<&str> = files.iter().map(|file| file.name.as_str()).collect();
    names.sort_unstable();
    names
}

fn bytes_of<'a>(files: &'a [SeedFile], name: &str) -> &'a [u8] {
    &files.iter().find(|file| file.name == name).unwrap().bytes
}

#[skuld::test]
fn a_file_in_a_subdirectory_written_by_a_guest_is_read_back_with_its_path(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("answer.img");
    let log = all_bytes();
    std::fs::write(
        &image,
        volume_with(
            &[file("winpe.cmd", b"echo")],
            &[("logs/setupact.log", &log), ("logs/deep/er/diag.txt", b"d")],
        ),
    )
    .unwrap();

    let files = read_files(&image, &["winpe.cmd", "logs/setupact.log", "logs/deep/er/diag.txt"]).unwrap();

    assert_eq!(
        names_of(&files),
        ["logs/deep/er/diag.txt", "logs/setupact.log", "winpe.cmd"]
    );
    assert_eq!(bytes_of(&files, "logs/setupact.log"), log);
    assert_eq!(bytes_of(&files, "logs/deep/er/diag.txt"), b"d");
    assert_eq!(bytes_of(&files, "winpe.cmd"), b"echo");
}

#[skuld::test]
fn only_the_paths_asked_for_are_read(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("answer.img");
    std::fs::write(
        &image,
        volume_with(
            &[file("winpe.cmd", b"echo")],
            &[("logs/a.log", b"a"), ("logs/b.log", b"b")],
        ),
    )
    .unwrap();

    let files = read_files(&image, &["logs/b.log"]).unwrap();

    assert_eq!(names_of(&files), ["logs/b.log"]);
}

#[skuld::test]
fn a_volume_left_dirty_by_the_guest_is_read(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("answer.img");
    let mut bytes = volume_with(&[file("winpe.cmd", b"echo")], &[("logs/setupact.log", b"log")]);
    // FAT12 boot sector, offset 37: the flags byte of the extended BIOS parameter block; bit 0 is "dirty".
    bytes[37] |= 1;
    assert!(
        FileSystem::new(Cursor::new(bytes.clone()), FsOptions::new())
            .unwrap()
            .read_status_flags()
            .unwrap()
            .dirty(),
        "the fixture is dirty"
    );
    std::fs::write(&image, bytes).unwrap();

    let files = read_files(&image, &["winpe.cmd", "logs/setupact.log"]).unwrap();

    assert_eq!(names_of(&files), ["logs/setupact.log", "winpe.cmd"]);
}

#[skuld::test]
fn a_path_the_guest_did_not_write_reads_as_absent_not_an_error(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("answer.img");
    std::fs::write(&image, volume_with(&[file("winpe.cmd", b"echo")], &[])).unwrap();

    let files = read_files(&image, &["winpe.cmd", "logs/setupact.log", "nothing/at/all.txt"]).unwrap();

    assert_eq!(names_of(&files), ["winpe.cmd"]);
}

#[skuld::test]
fn a_path_through_a_file_or_to_a_directory_is_an_error(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("answer.img");
    std::fs::write(
        &image,
        volume_with(&[file("winpe.cmd", b"echo")], &[("logs/a.log", b"a")]),
    )
    .unwrap();

    for path in ["winpe.cmd/inside", "logs"] {
        let error = read_files(&image, &[path]).expect_err(path);
        assert_eq!(error.kind(), crate::ErrorKind::Io, "{path}: {error}");
        assert!(error.message().contains("answer.img"), "{path}: {error}");
    }
}

#[skuld::test]
fn a_path_that_walks_up_or_names_nothing_is_refused_before_the_image_is_read(#[fixture(unicode_home)] home: &Path) {
    // The image does not exist: a refused path must not get as far as reading it.
    let image = home.join("missing.img");
    for path in [
        "",
        "/",
        "logs//a.log",
        "logs/../winpe.cmd",
        "./winpe.cmd",
        "logs\\a.log",
    ] {
        let error = read_files(&image, &[path]).expect_err(path);
        assert_eq!(error.kind(), crate::ErrorKind::Internal, "{path:?}: {error}");
        assert_eq!(
            error.message(),
            format!("`{path}` cannot be read from an image: it is empty, or has an empty, `.` or `..` part, or a `\\`"),
            "{path:?}"
        );
    }
}

#[skuld::test]
fn read_files_returns_what_write_cidata_image_wrote(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("seed.img");
    write_cidata_image(
        &TestHost,
        &[
            file("user-data", &all_bytes()),
            file("with space", b"s"),
            file("empty", b""),
        ],
        MediaKind::Disk,
        true,
        0,
        &image,
    )
    .unwrap();

    let files = read_files(&image, &["empty", "user-data", "with space"]).unwrap();

    assert_eq!(names_of(&files), ["empty", "user-data", "with space"]);
    assert_eq!(bytes_of(&files, "user-data"), all_bytes());
}

#[skuld::test]
fn an_image_that_is_not_a_fat_volume_is_an_error_naming_it(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("not-fat.img");
    std::fs::write(&image, vec![0u8; 4096]).unwrap();
    let error = read_files(&image, &["a"]).expect_err("zeros are no volume");
    assert_eq!(error.kind(), crate::ErrorKind::Io);
    assert!(error.message().contains("not-fat.img"), "{}", error.message());
}

#[skuld::test]
fn a_missing_image_is_an_io_error_naming_it(#[fixture(unicode_home)] home: &Path) {
    let error = read_files(&home.join("missing.img"), &["a"]).expect_err("there is no such file");
    assert_eq!(error.kind(), crate::ErrorKind::Io);
    assert!(error.message().contains("missing.img"), "{}", error.message());
}

// read_files of a volume a guest corrupted ----------------------------------------------------------------------------

/// Where the 32-byte entry named `short` (11 bytes, space padded) starts in `image`.
fn entry_at(image: &[u8], short: &[u8; 11]) -> usize {
    (0..image.len() - 32)
        .step_by(32)
        .find(|at| image[*at..*at + 11] == short[..] && image[*at + 11] != 0x0F)
        .unwrap_or_else(|| panic!("no entry {:?}", String::from_utf8_lossy(short)))
}

fn word(image: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([image[at], image[at + 1]]))
}

/// Makes the table of `image`, a volume of `fat_type`, say that `cluster` is followed by `next`, in every copy of the
/// table, or in copy `only` alone.
fn chain_cluster_in(image: &mut [u8], fat_type: fatfs::FatType, cluster: usize, next: usize, only: Option<usize>) {
    let (reserved, fats, sector) = (word(image, 14), usize::from(image[16]), word(image, 11));
    let per_fat = match fat_type {
        fatfs::FatType::Fat32 => word(image, 36) | word(image, 38) << 16,
        _ => word(image, 22),
    };
    for copy in (0..fats).filter(|copy| only.is_none_or(|only| only == *copy)) {
        let table = (reserved + copy * per_fat) * sector;
        match fat_type {
            fatfs::FatType::Fat12 => {
                let at = table + cluster * 3 / 2;
                let old = word(image, at);
                let new = if cluster.is_multiple_of(2) {
                    (old & 0xF000) | next
                } else {
                    (old & 0x000F) | (next << 4)
                };
                image[at..at + 2].copy_from_slice(&(new as u16).to_le_bytes());
            }
            fatfs::FatType::Fat16 => {
                let at = table + cluster * 2;
                image[at..at + 2].copy_from_slice(&(next as u16).to_le_bytes());
            }
            fatfs::FatType::Fat32 => {
                let at = table + cluster * 4;
                image[at..at + 4].copy_from_slice(&(next as u32).to_le_bytes());
            }
        }
    }
}

fn chain_cluster_to(image: &mut [u8], fat_type: fatfs::FatType, cluster: usize, next: usize) {
    chain_cluster_in(image, fat_type, cluster, next, None);
}

fn first_cluster_of(image: &[u8], entry: usize) -> usize {
    word(image, entry + 26) | word(image, entry + 20) << 16
}

fn set_first_cluster(image: &mut [u8], entry: usize, cluster: usize) {
    image[entry + 26..entry + 28].copy_from_slice(&(cluster as u16).to_le_bytes());
}

/// Rewrites every slot of `cluster` as a deleted entry: a directory that never says it has ended.
fn delete_every_entry_in(image: &mut [u8], cluster: usize) {
    let (reserved, fats, per_fat, sector) = (
        word(image, 14),
        usize::from(image[16]),
        word(image, 22),
        word(image, 11),
    );
    let root = (word(image, 17) * 32).next_multiple_of(sector);
    let cluster_size = usize::from(image[13]) * sector;
    let start = (reserved + fats * per_fat) * sector + root + (cluster - 2) * cluster_size;
    for slot in image[start..start + cluster_size].chunks_mut(32) {
        slot.fill(0);
        slot[0] = 0xE5;
    }
}

/// How many clusters the volume `image` has.
fn clusters_of(image: &[u8]) -> usize {
    let fs = FileSystem::new(Cursor::new(image.to_vec()), FsOptions::new()).unwrap();
    fs.stats().unwrap().total_clusters() as usize
}

/// A volume of `fat_type` and `megabytes` holding the one-byte file `LOOP`.
fn volume_of(fat_type: fatfs::FatType, megabytes: usize) -> Vec<u8> {
    let mut disk = Cursor::new(vec![0u8; megabytes << 20]);
    let options = fatfs::FormatVolumeOptions::new()
        .fat_type(fat_type)
        .total_sectors(((megabytes << 20) / 512) as u32);
    fatfs::format_volume(&mut disk, options).unwrap();
    let fs = FileSystem::new(&mut disk, FsOptions::new()).unwrap();
    assert_eq!(fs.fat_type(), fat_type);
    fs.root_dir().create_file("LOOP").unwrap().write_all(b"x").unwrap();
    fs.unmount().unwrap();
    disk.into_inner()
}

const THE_THREE: [(fatfs::FatType, usize); 3] = [
    (fatfs::FatType::Fat12, 1),
    (fatfs::FatType::Fat16, 20),
    (fatfs::FatType::Fat32, 40),
];

fn read_loop(home: &Path, bytes: Vec<u8>) -> crate::Result<Vec<SeedFile>> {
    let image = home.join("answer.img");
    std::fs::write(&image, bytes).unwrap();
    read_files(&image, &["LOOP"])
}

#[skuld::test]
fn a_directory_that_leads_back_to_an_ancestor_costs_only_the_paths_asked(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("answer.img");
    let mut bytes = volume_with(
        &[file("winpe.cmd", b"echo")],
        &[("logs/sub/y.txt", b"y"), ("logs/x.txt", b"x")],
    );
    let logs = first_cluster_of(&bytes, entry_at(&bytes, b"LOGS       "));
    let sub = entry_at(&bytes, b"SUB        ");
    set_first_cluster(&mut bytes, sub, logs);
    std::fs::write(&image, bytes).unwrap();

    // `logs/sub` is `logs`, so each `sub` in a path comes back to it; the path decides how far that goes.
    let files = read_files(
        &image,
        &[
            "logs/x.txt",
            "logs/sub/sub/sub/x.txt",
            "logs/sub/sub/missing.txt",
            "logs/sub/y.txt",
        ],
    )
    .unwrap();

    assert_eq!(names_of(&files), ["logs/sub/sub/sub/x.txt", "logs/x.txt"]);
    assert_eq!(bytes_of(&files, "logs/sub/sub/sub/x.txt"), b"x");
}

#[skuld::test]
fn a_directory_whose_clusters_loop_is_an_error_not_an_endless_read(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("answer.img");
    let mut bytes = volume_with(&[file("winpe.cmd", b"echo")], &[("logs/x.txt", b"x")]);
    let logs = first_cluster_of(&bytes, entry_at(&bytes, b"LOGS       "));
    // No entry is live and none ends the directory, so only the table can tell that the reading never ends.
    delete_every_entry_in(&mut bytes, logs);
    chain_cluster_to(&mut bytes, fatfs::FatType::Fat12, logs, logs);
    std::fs::write(&image, bytes).unwrap();

    let error = read_files(&image, &["logs/nothing.txt"]).expect_err("the directory has no end");

    assert_eq!(error.kind(), crate::ErrorKind::Io, "{error}");
    assert!(
        error
            .message()
            .contains(&format!("cluster {logs} leads back to itself")),
        "{error}"
    );
}

#[skuld::test]
fn a_file_whose_clusters_loop_is_an_error_not_a_read_of_its_claimed_size(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("answer.img");
    let mut bytes = volume_with(&[file("winpe.cmd", b"echo")], &[("logs/x.txt", b"x")]);
    let entry = entry_at(&bytes, b"X       TXT");
    let cluster = first_cluster_of(&bytes, entry);
    chain_cluster_to(&mut bytes, fatfs::FatType::Fat12, cluster, cluster);
    // The most a 32-bit directory entry can claim: 4 GiB less one byte.
    bytes[entry + 28..entry + 32].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&image, bytes).unwrap();

    let error = read_files(&image, &["logs/x.txt"]).expect_err("the file has no end");

    assert_eq!(error.kind(), crate::ErrorKind::Io, "{error}");
    assert!(
        error
            .message()
            .contains(&format!("cluster {cluster} leads back to itself")),
        "{error}"
    );
}

#[skuld::test]
fn a_file_claiming_more_than_its_clusters_is_read_as_far_as_they_go(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("answer.img");
    let mut bytes = volume_with(&[file("winpe.cmd", b"echo")], &[("logs/x.txt", b"x")]);
    let entry = entry_at(&bytes, b"X       TXT");
    bytes[entry + 28..entry + 32].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&image, bytes).unwrap();

    let files = read_files(&image, &["logs/x.txt"]).unwrap();

    assert!(files[0].bytes.starts_with(b"x"));
    assert!(files[0].bytes.len() <= 1 << 20, "{} bytes", files[0].bytes.len());
}

#[skuld::test]
fn a_directory_with_many_entries_is_not_mistaken_for_a_loop(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("answer.img");
    let names: Vec<String> = (0..200).map(|n| format!("f{n}.txt")).collect();
    let nested: Vec<(String, &[u8])> = names.iter().map(|n| (format!("logs/{n}"), &b"x"[..])).collect();
    let nested: Vec<(&str, &[u8])> = nested.iter().map(|(n, b)| (n.as_str(), *b)).collect();
    std::fs::write(&image, volume_with(&[file("winpe.cmd", b"echo")], &nested)).unwrap();

    let files = read_files(&image, &["logs/f0.txt", "logs/f199.txt"]).unwrap();

    assert_eq!(names_of(&files), ["logs/f0.txt", "logs/f199.txt"]);
}

#[skuld::test]
fn a_file_whose_clusters_loop_is_an_error_on_every_fat_type(#[fixture(unicode_home)] home: &Path) {
    for (fat_type, megabytes) in THE_THREE {
        let mut bytes = volume_of(fat_type, megabytes);
        let cluster = first_cluster_of(&bytes, entry_at(&bytes, b"LOOP       "));
        chain_cluster_to(&mut bytes, fat_type, cluster, cluster);

        let error = read_loop(home, bytes).expect_err("the file has no end");

        assert!(
            error
                .message()
                .contains(&format!("cluster {cluster} leads back to itself")),
            "{fat_type:?}: {error}"
        );
    }
}

#[skuld::test]
fn a_table_value_fatfs_follows_outside_the_volume_is_refused_on_every_fat_type(#[fixture(unicode_home)] home: &Path) {
    for (fat_type, megabytes) in THE_THREE {
        let clusters = clusters_of(&volume_of(fat_type, megabytes));
        // 1 is below the first cluster; `clusters + 2` is the first number past the last; the largest value fatfs
        // still reads as a cluster number is the one before the bad-cluster mark.
        let largest = match fat_type {
            fatfs::FatType::Fat12 => 0xFF6,
            fatfs::FatType::Fat16 => 0xFFF6,
            fatfs::FatType::Fat32 => 0x0FFF_FFF6,
        };
        for next in [1, clusters + 2, largest] {
            let mut bytes = volume_of(fat_type, megabytes);
            let cluster = first_cluster_of(&bytes, entry_at(&bytes, b"LOOP       "));
            chain_cluster_to(&mut bytes, fat_type, cluster, next);

            let error = read_loop(home, bytes).expect_err("a chain that leaves the volume");

            assert_eq!(error.kind(), crate::ErrorKind::Io, "{fat_type:?} -> {next}: {error}");
            assert!(
                error.message().contains(&format!(
                    "cluster {cluster} is followed by cluster {next}, outside the {clusters} clusters"
                )),
                "{fat_type:?} -> {next}: {error}"
            );
        }
    }
}

#[skuld::test]
fn a_chain_the_table_ends_in_any_way_fatfs_ends_it_is_not_corrupt(#[fixture(unicode_home)] home: &Path) {
    for (fat_type, megabytes) in THE_THREE {
        let (free, bad, end) = match fat_type {
            fatfs::FatType::Fat12 => (0, 0xFF7, 0xFF8),
            fatfs::FatType::Fat16 => (0, 0xFFF7, 0xFFF8),
            fatfs::FatType::Fat32 => (0, 0x0FFF_FFF7, 0x0FFF_FFF8),
        };
        for next in [free, bad, end] {
            let mut bytes = volume_of(fat_type, megabytes);
            let cluster = first_cluster_of(&bytes, entry_at(&bytes, b"LOOP       "));
            chain_cluster_to(&mut bytes, fat_type, cluster, next);

            let files = read_loop(home, bytes).unwrap_or_else(|e| panic!("{fat_type:?} -> {next:#x}: {e}"));

            assert_eq!(bytes_of(&files, "LOOP"), b"x", "{fat_type:?} -> {next:#x}");
        }
    }
}

#[skuld::test]
fn a_file_that_starts_past_the_last_cluster_and_loops_there_is_refused(#[fixture(unicode_home)] home: &Path) {
    for (fat_type, megabytes) in [(fatfs::FatType::Fat12, 1), (fatfs::FatType::Fat16, 20)] {
        let mut bytes = volume_of(fat_type, megabytes);
        let clusters = clusters_of(&bytes);
        let entry = entry_at(&bytes, b"LOOP       ");
        let past = clusters + 3;
        set_first_cluster(&mut bytes, entry, past);
        chain_cluster_to(&mut bytes, fat_type, past, past);

        let error = read_loop(home, bytes).expect_err("a chain that never ends, outside the volume");

        assert_eq!(error.kind(), crate::ErrorKind::Io, "{fat_type:?}: {error}");
        assert!(
            error
                .message()
                .contains(&format!("cluster {past} is followed by cluster {past}")),
            "{fat_type:?}: {error}"
        );
    }
}

/// FAT32 boot sector, offset 40: the flags of the table; bit 7 turns mirroring off, the low four bits name the copy in
/// use.
fn use_one_table_copy(image: &mut [u8], copy: u16) {
    let flags = 0x80 | copy;
    image[40..42].copy_from_slice(&flags.to_le_bytes());
}

#[skuld::test]
fn the_table_copy_fatfs_reads_is_the_one_that_is_checked(#[fixture(unicode_home)] home: &Path) {
    let fat_type = fatfs::FatType::Fat32;
    let fresh = || {
        let mut bytes = volume_of(fat_type, 40);
        assert_eq!(bytes[16], 2, "two copies of the table");
        use_one_table_copy(&mut bytes, 1);
        bytes
    };
    let fixture = fresh();
    let cluster = first_cluster_of(&fixture, entry_at(&fixture, b"LOOP       "));

    // A loop in the copy in use is refused, as it would be with mirroring on.
    let mut in_use = fresh();
    chain_cluster_in(&mut in_use, fat_type, cluster, cluster, Some(1));
    let error = read_loop(home, in_use).expect_err("the copy in use loops");
    assert!(
        error
            .message()
            .contains(&format!("cluster {cluster} leads back to itself")),
        "{error}"
    );

    // A loop in the other copy is not read by fatfs, so it is no fault of the volume.
    let mut unused = fresh();
    chain_cluster_in(&mut unused, fat_type, cluster, cluster, Some(0));
    let files = read_loop(home, unused).unwrap();
    assert_eq!(bytes_of(&files, "LOOP"), b"x");
}
