use std::io::{self, Read, Write};
use std::path::Path;

use crate::ErrorKind;
use crate::copy::archive::{append_file, append_symlink, check_one_component_per_segment};

const NAME: &str = "dir/\u{416}.txt";
const DEST: &str = "out/devvm-copy-x.tar";

/// Archives `bytes` under a header that declares `declared` bytes.
fn append(declared: u64, bytes: &[u8]) -> crate::Result<crate::Sha256> {
    let mut builder = tar::Builder::new(Vec::new());
    append_file(&mut builder, Path::new(DEST), NAME, 0o644, declared, bytes)
}

/// Accepts `budget` bytes, then fails every write with `kind`.
struct FailingWriter {
    budget: usize,
    kind: io::ErrorKind,
}

impl Write for FailingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.budget == 0 {
            return Err(io::Error::new(self.kind, "the device refused"));
        }
        let n = buf.len().min(self.budget);
        self.budget -= n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A reader whose every read fails with `kind`.
struct FailingReader(io::ErrorKind);

impl Read for FailingReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::new(self.0, "the device refused"))
    }
}

#[skuld::test]
fn a_file_that_shrinks_while_it_is_read_is_an_error_naming_it() {
    let error = append(10, b"short").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains(NAME), "{error}");
}

#[skuld::test]
fn a_file_that_grows_while_it_is_read_is_an_error_naming_it() {
    let error = append(3, b"longer than declared").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains(NAME), "{error}");
}

#[skuld::test]
fn a_name_the_host_reads_as_one_component_per_slash_segment_is_accepted() {
    for name in ["a", "a/b", "dir/\u{416}.txt", "a b/c d", "-dash/x"] {
        assert!(check_one_component_per_segment(name).is_ok(), "{name:?}");
    }
}

#[skuld::test]
fn a_backslash_is_refused_only_on_a_host_where_it_separates_components() {
    let result = check_one_component_per_segment("dir\\file");
    if std::path::MAIN_SEPARATOR == '\\' {
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Recipe);
        assert!(error.message().contains(r"dir\\file"), "{error}");
    } else {
        assert!(result.is_ok());
    }
}

#[skuld::test]
fn a_file_of_the_declared_size_is_hashed_and_archived() {
    use sha2::Digest;
    let mut builder = tar::Builder::new(Vec::new());
    let sha = append_file(&mut builder, Path::new(DEST), "f", 0o644, 5, &b"hello"[..]).unwrap();
    assert_eq!(sha.as_bytes().as_slice(), sha2::Sha256::digest(b"hello").as_slice());
    let archive = builder.into_inner().unwrap();
    let mut tar = tar::Archive::new(&archive[..]);
    let mut entry = tar.entries().unwrap().next().unwrap().unwrap();
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"hello");
}

#[skuld::test]
fn a_full_disk_writing_the_archive_is_disk_full_naming_the_archive() {
    for kind in [io::ErrorKind::StorageFull, io::ErrorKind::QuotaExceeded] {
        // The header fits, the content does not.
        let mut builder = tar::Builder::new(FailingWriter { budget: 600, kind });
        let error = append_file(&mut builder, Path::new(DEST), NAME, 0o644, 2000, &[7u8; 2000][..]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::DiskFull, "{kind:?}: {error}");
        assert!(error.message().contains(DEST), "{kind:?}: {error}");
    }
}

#[skuld::test]
fn another_failure_writing_the_archive_is_io_naming_the_archive() {
    let mut builder = tar::Builder::new(FailingWriter {
        budget: 600,
        kind: io::ErrorKind::PermissionDenied,
    });
    let error = append_file(&mut builder, Path::new(DEST), NAME, 0o644, 2000, &[7u8; 2000][..]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains(DEST), "{error}");
}

#[skuld::test]
fn a_failure_reading_the_file_is_io_naming_the_file_even_when_it_looks_like_a_full_disk() {
    let mut builder = tar::Builder::new(Vec::new());
    let error = append_file(
        &mut builder,
        Path::new(DEST),
        NAME,
        0o644,
        10,
        FailingReader(io::ErrorKind::StorageFull),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains(NAME), "{error}");
}

/// A reader that fails once with `Interrupted`, which `io::copy` retries, and then reads `rest`.
struct InterruptedOnce<'a> {
    interrupted: bool,
    rest: &'a [u8],
}

impl Read for InterruptedOnce<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.interrupted {
            self.interrupted = true;
            return Err(io::ErrorKind::Interrupted.into());
        }
        self.rest.read(buf)
    }
}

#[skuld::test]
fn a_read_that_is_retried_is_not_blamed_when_the_disk_then_fills() {
    let mut builder = tar::Builder::new(FailingWriter {
        budget: 600,
        kind: io::ErrorKind::StorageFull,
    });
    let reader = InterruptedOnce {
        interrupted: false,
        rest: &[7u8; 2000],
    };
    let error = append_file(&mut builder, Path::new(DEST), NAME, 0o644, 2000, reader).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::DiskFull, "{error}");
    assert!(error.message().contains(DEST), "{error}");
}

/// Archives a symlink `path` -> `target` and returns the entry's link name as the tar holds it.
fn link_name_in_tar(path: &str, target: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    append_symlink(&mut builder, Path::new(DEST), path, target).unwrap();
    let archive = builder.into_inner().unwrap();
    let mut tar = tar::Archive::new(&archive[..]);
    let mut entries = tar.entries().unwrap();
    let entry = entries.next().unwrap().unwrap();
    assert_eq!(entry.header().entry_type(), tar::EntryType::Symlink);
    assert_eq!(&*entry.path_bytes(), path.as_bytes());
    let name = entry.link_name_bytes().expect("a symlink has a link name").into_owned();
    assert!(entries.next().is_none(), "one entry for one link");
    name
}

#[skuld::test]
fn a_symlink_target_is_archived_byte_for_byte() {
    let long_path = format!("{}/f", "d".repeat(150));
    let targets = [
        "a/b".to_owned(),
        "a//b".to_owned(),
        "a/./b".to_owned(),
        r"a\b".to_owned(),
        r"C:\x".to_owned(),
        "./a".to_owned(),
        "a/../b".to_owned(),
        "dir/".to_owned(),
        "t".repeat(99),
        "t".repeat(100),
        "t".repeat(101),
        "d/".repeat(75),
        // 120 bytes whose 99th byte is inside a character.
        "\u{e9}".repeat(60),
    ];
    for target in &targets {
        for path in [NAME, long_path.as_str()] {
            assert_eq!(
                link_name_in_tar(path, target),
                target.as_bytes(),
                "{path:?} -> {target:?}"
            );
        }
    }
}

#[skuld::test]
fn a_symlink_target_with_a_nul_is_refused_naming_the_link() {
    let mut builder = tar::Builder::new(Vec::new());
    let error = append_symlink(&mut builder, Path::new(DEST), NAME, "a\0b").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains(NAME), "{error}");
}
