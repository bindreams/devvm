use std::io::{self, Read, Write};
use std::path::Path;

use crate::ErrorKind;
use crate::copy::archive::append_file;

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
