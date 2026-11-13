use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256 as Sha2};

use super::{IsoMemo, Known};
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::host::TestHost;
use crate::{Dirs, ErrorKind, Sha256};

fn digest(bytes: &[u8]) -> Sha256 {
    Sha256::from_bytes(Sha2::digest(bytes).into())
}

fn iso_with(home: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = home.join(name);
    std::fs::write(&path, bytes).unwrap();
    crate::host::current().set_mtime(&path, 1_700_000_000).unwrap();
    path
}

fn hashings(progress: &MemoryProgress) -> usize {
    progress
        .messages()
        .iter()
        .filter(|message| message.starts_with("hashing ") && message.contains(": 0 of "))
        .count()
}

#[skuld::test]
fn the_hash_is_the_sha256_of_the_content_and_is_reused_while_nothing_changed(#[fixture(unicode_home)] home: &Path) {
    let iso = iso_with(home, "win 😀.iso", b"windows");
    let memo = IsoMemo::new(&Dirs::under(home));
    let progress = MemoryProgress::default();
    assert_eq!(memo.sha256(&TestHost, &iso, &progress).unwrap(), digest(b"windows"));
    assert_eq!(hashings(&progress), 1);
    assert_eq!(memo.sha256(&TestHost, &iso, &progress).unwrap(), digest(b"windows"));
    assert_eq!(hashings(&progress), 1, "the second call read the memo");
    // Another process's memo object reads the same file.
    assert_eq!(
        IsoMemo::new(&Dirs::under(home))
            .sha256(&TestHost, &iso, &progress)
            .unwrap(),
        digest(b"windows")
    );
    assert_eq!(hashings(&progress), 1);
}

#[skuld::test]
fn any_one_of_the_four_changing_rehashes(#[fixture(unicode_home)] home: &Path) {
    let memo = IsoMemo::new(&Dirs::under(home));
    let progress = MemoryProgress::default();
    let iso = iso_with(home, "a.iso", b"first");
    memo.sha256(&TestHost, &iso, &progress).unwrap();
    assert_eq!(hashings(&progress), 1);

    // The modification time alone.
    crate::host::current().set_mtime(&iso, 1_700_000_001).unwrap();
    assert_eq!(memo.sha256(&TestHost, &iso, &progress).unwrap(), digest(b"first"));
    assert_eq!(hashings(&progress), 2, "mtime");

    // The size alone: same mtime as before, one byte more.
    std::fs::write(&iso, b"first!").unwrap();
    crate::host::current().set_mtime(&iso, 1_700_000_001).unwrap();
    assert_eq!(memo.sha256(&TestHost, &iso, &progress).unwrap(), digest(b"first!"));
    assert_eq!(hashings(&progress), 3, "size");

    // The file id alone: another file of the same size and time takes the name. The copy exists beside the
    // original, so the two have different ids even where a freed id is reused at once.
    let other = iso_with(home, "b.iso", b"first?");
    crate::host::current().set_mtime(&other, 1_700_000_001).unwrap();
    std::fs::rename(&other, &iso).unwrap();
    assert_eq!(memo.sha256(&TestHost, &iso, &progress).unwrap(), digest(b"first?"));
    assert_eq!(hashings(&progress), 4, "file id");

    // The path alone: equal bytes at another path are hashed again, and each path keeps its own memo.
    let copy = iso_with(home, "c.iso", b"first?");
    assert_eq!(memo.sha256(&TestHost, &copy, &progress).unwrap(), digest(b"first?"));
    assert_eq!(hashings(&progress), 5, "path");
    memo.sha256(&TestHost, &iso, &progress).unwrap();
    memo.sha256(&TestHost, &copy, &progress).unwrap();
    assert_eq!(hashings(&progress), 5);
}

#[skuld::test]
fn a_corrupt_memo_is_quarantined_and_rebuilt(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let memo = IsoMemo::new(&dirs);
    let progress = MemoryProgress::default();
    let iso = iso_with(home, "a.iso", b"windows");
    memo.sha256(&TestHost, &iso, &progress).unwrap();
    let [file] = std::fs::read_dir(dirs.iso_memo())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    std::fs::write(&file, b"{ torn").unwrap();

    assert_eq!(memo.sha256(&TestHost, &iso, &progress).unwrap(), digest(b"windows"));
    assert_eq!(hashings(&progress), 2, "the torn memo counts as none");
    let quarantined: Vec<_> = std::fs::read_dir(dirs.quarantine()).unwrap().collect();
    assert_eq!(quarantined.len(), 1);
    assert_eq!(
        std::fs::read(quarantined[0].as_ref().unwrap().path()).unwrap(),
        b"{ torn"
    );
    assert!(
        progress.messages().iter().any(|message| message.contains("unreadable")),
        "{:?}",
        progress.messages()
    );
    memo.sha256(&TestHost, &iso, &progress).unwrap();
    assert_eq!(hashings(&progress), 2, "the memo made again is used");
}

#[skuld::test]
fn a_missing_iso_is_not_found_naming_it(#[fixture(unicode_home)] home: &Path) {
    let memo = IsoMemo::new(&Dirs::under(home));
    let missing = home.join("no such.iso");
    for error in [
        memo.sha256(&TestHost, &missing, &MemoryProgress::default())
            .unwrap_err(),
        memo.check_unchanged(&TestHost, &missing, digest(b""), &MemoryProgress::default())
            .unwrap_err(),
        memo.rehash(&missing, &MemoryProgress::default()).unwrap_err(),
    ] {
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(error.message().contains("no such.iso"), "{error}");
    }
}

#[skuld::test]
fn an_iso_that_changed_after_it_was_keyed_is_named_and_one_that_did_not_passes(#[fixture(unicode_home)] home: &Path) {
    let memo = IsoMemo::new(&Dirs::under(home));
    let progress = MemoryProgress::default();
    let iso = iso_with(home, "keyed.iso", b"windows");
    let keyed = memo.sha256(&TestHost, &iso, &progress).unwrap();
    memo.check_unchanged(&TestHost, &iso, keyed, &progress).unwrap();

    std::fs::write(&iso, b"windows, patched").unwrap();
    let error = memo.check_unchanged(&TestHost, &iso, keyed, &progress).unwrap_err();
    assert!(
        error
            .message()
            .contains("keyed.iso changed after the build was keyed by it"),
        "{error}"
    );
}

#[skuld::test]
fn an_iso_hashed_again_with_the_same_content_is_still_the_keyed_one(#[fixture(unicode_home)] home: &Path) {
    // A touched ISO has a new modification time but the hash it was keyed by: the check asks for the memo, so the
    // caller that keyed it must have hashed it again first.
    let memo = IsoMemo::new(&Dirs::under(home));
    let progress = MemoryProgress::default();
    let iso = iso_with(home, "touched.iso", b"windows");
    let keyed = memo.sha256(&TestHost, &iso, &progress).unwrap();
    crate::host::current().set_mtime(&iso, 1_700_000_500).unwrap();
    assert!(memo.check_unchanged(&TestHost, &iso, keyed, &progress).is_err());
    memo.sha256(&TestHost, &iso, &progress).unwrap();
    memo.check_unchanged(&TestHost, &iso, keyed, &progress).unwrap();
}

#[skuld::test]
fn the_known_isos_are_listed_by_path_and_an_unreadable_memo_is_listed_as_such(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let memo = IsoMemo::new(&dirs);
    let progress = MemoryProgress::default();
    assert_eq!(memo.known().unwrap(), []);
    let b = iso_with(home, "b.iso", b"bb");
    let a = iso_with(home, "a.iso", b"aa");
    memo.sha256(&TestHost, &b, &progress).unwrap();
    memo.sha256(&TestHost, &a, &progress).unwrap();
    std::fs::write(dirs.iso_memo().join("junk.json"), b"[]").unwrap();
    std::fs::write(dirs.iso_memo().join("note.txt"), b"not a memo").unwrap();

    let known = memo.known().unwrap();
    let isos: Vec<_> = known
        .iter()
        .filter_map(|known| match known {
            Known::Iso { path, sha256 } => Some((path.clone(), *sha256)),
            Known::Unreadable { .. } => None,
        })
        .collect();
    assert_eq!(isos, [(a, digest(b"aa")), (b, digest(b"bb"))]);
    assert_eq!(known.len(), 3);
    assert!(
        known
            .iter()
            .any(|known| matches!(known, Known::Unreadable { file, .. } if file.ends_with("junk.json")))
    );
}

#[skuld::test]
fn rehash_reads_the_iso_now_and_remembers_nothing(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let memo = IsoMemo::new(&dirs);
    let iso = iso_with(home, "a.iso", b"windows");
    assert_eq!(
        memo.rehash(&iso, &MemoryProgress::default()).unwrap(),
        digest(b"windows")
    );
    assert!(!dirs.iso_memo().exists());
}
