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
    assert_eq!(image_size(LARGEST_FAT12_CONTENTS).unwrap(), 133_885_952);

    let error = image_size(LARGEST_FAT12_CONTENTS + 1).expect_err("over the limit");
    assert_eq!(error.kind(), crate::ErrorKind::Internal, "{error}");
    assert!(error.message().contains("66418689 bytes"), "{error}");
    assert!(error.message().contains("133886464 bytes"), "{error}");
    assert!(error.message().contains("FAT12"), "{error}");

    for contents in [1 << 40, u64::MAX] {
        let error = image_size(contents).expect_err("over the limit");
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

    let files = read_files(&image).unwrap();

    assert_eq!(
        names_of(&files),
        ["logs/deep/er/diag.txt", "logs/setupact.log", "winpe.cmd"]
    );
    let by_name = |name: &str| &files.iter().find(|file| file.name == name).unwrap().bytes;
    assert_eq!(*by_name("logs/setupact.log"), log);
    assert_eq!(*by_name("logs/deep/er/diag.txt"), b"d");
    assert_eq!(*by_name("winpe.cmd"), b"echo");
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

    let files = read_files(&image).unwrap();

    assert_eq!(names_of(&files), ["logs/setupact.log", "winpe.cmd"]);
}

#[skuld::test]
fn an_image_with_no_logs_directory_reads_as_no_logs_not_an_error(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("answer.img");
    std::fs::write(&image, volume_with(&[file("winpe.cmd", b"echo")], &[])).unwrap();

    let files = read_files(&image).unwrap();

    assert_eq!(names_of(&files), ["winpe.cmd"]);
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

    let files = read_files(&image).unwrap();

    assert_eq!(names_of(&files), ["empty", "user-data", "with space"]);
    assert_eq!(
        files.iter().find(|file| file.name == "user-data").unwrap().bytes,
        all_bytes()
    );
}

#[skuld::test]
fn an_image_that_is_not_a_fat_volume_is_an_error_naming_it(#[fixture(unicode_home)] home: &Path) {
    let image = home.join("not-fat.img");
    std::fs::write(&image, vec![0u8; 4096]).unwrap();
    let error = read_files(&image).expect_err("zeros are no volume");
    assert_eq!(error.kind(), crate::ErrorKind::Io);
    assert!(error.message().contains("not-fat.img"), "{}", error.message());
}

#[skuld::test]
fn a_missing_image_is_an_io_error_naming_it(#[fixture(unicode_home)] home: &Path) {
    let error = read_files(&home.join("missing.img")).expect_err("there is no such file");
    assert_eq!(error.kind(), crate::ErrorKind::Io);
    assert!(error.message().contains("missing.img"), "{}", error.message());
}
