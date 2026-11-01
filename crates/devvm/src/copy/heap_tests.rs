//! The snapshot of a file past 4 GiB. The heap number only means something in a process of its own, so the test runs
//! the lib's test binary as the helper `snapshot-big` and asserts that it exited 0 (the pattern of
//! `access/heap_tests.rs`, with its counting allocator).

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::access::heap_tests::{LIMIT, make_big_source};
use crate::copy::copy_tests::{add_all, new_repo, paths, take};
use crate::recipe::hash::EntryContent;
use crate::test_support::alloc::peak_bytes;
use crate::test_support::helper;
use crate::test_support::sshd::{BIG_SIZE, big_file_digest_from, expected_big_digest};

/// The SHA-256 of the file at `path`, read in pieces.
fn sha256_of(path: &Path) -> crate::Sha256 {
    let mut file = std::fs::File::open(path).expect("the source file");
    let mut hasher = Sha256::new();
    let mut piece = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut piece).expect("the source file reads");
        if n == 0 {
            break;
        }
        hasher.update(&piece[..n]);
    }
    crate::Sha256::from_bytes(hasher.finalize().into())
}

const BLOCK: u64 = 512;

/// The helper's entry point: `snapshot-big`.
pub fn snapshot_big(_: &[String]) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let repo = new_repo(dir.path(), "repo");
    make_big_source(&repo.join("big.bin"));
    add_all(&repo);

    let taken = take(&repo, false, dir.path()).expect("the snapshot succeeds");

    assert_eq!(paths(&taken.snapshot), ["big.bin"]);
    let entry = &taken.snapshot.manifest.entries()[0];
    assert_eq!(entry.mode, 0o100644);
    let content = entry.content.clone();
    // One header, the data padded to whole blocks, and the two blocks that end a tar: the length is exact.
    let expected_len = BLOCK + BIG_SIZE.div_ceil(BLOCK) * BLOCK + 2 * BLOCK;
    let archive = &taken.snapshot.archive;
    assert_eq!(std::fs::metadata(archive).unwrap().len(), expected_len);
    // This also covers the bytes past 4 GiB.
    assert_eq!(
        big_file_digest_from(archive, BLOCK).unwrap(),
        expected_big_digest(),
        "the archived content is the file's"
    );

    let peak = peak_bytes();
    assert!(peak < LIMIT, "peak heap {peak} bytes is not below {LIMIT}");

    // After the heap is measured: the hashing buffer is the test's. Every byte of the source, not only the windows.
    assert_eq!(
        content,
        EntryContent::FileSha256(sha256_of(&repo.join("big.bin"))),
        "the manifest hash is the SHA-256 of the file"
    );
}

#[skuld::test]
fn a_4_gib_plus_one_file_is_archived_and_hashed_without_buffering() {
    let output = helper::command("snapshot-big", &[])
        .output()
        .expect("the helper starts");
    assert!(
        output.status.success(),
        "snapshot-big failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
