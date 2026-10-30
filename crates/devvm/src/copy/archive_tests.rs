use std::io::Read;

use crate::ErrorKind;
use crate::copy::archive::append_file;

const NAME: &str = "dir/\u{416}.txt";

/// Archives `bytes` under a header that declares `declared` bytes.
fn append(declared: u64, bytes: &[u8]) -> crate::Result<crate::Sha256> {
    let mut builder = tar::Builder::new(Vec::new());
    append_file(&mut builder, NAME, 0o644, declared, bytes)
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
    let sha = append_file(&mut builder, "f", 0o644, 5, &b"hello"[..]).unwrap();
    assert_eq!(sha.as_bytes().as_slice(), sha2::Sha256::digest(b"hello").as_slice());
    let archive = builder.into_inner().unwrap();
    let mut tar = tar::Archive::new(&archive[..]);
    let mut entry = tar.entries().unwrap().next().unwrap().unwrap();
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"hello");
}
