use std::cell::Cell;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256 as Sha2};

use super::{IsoMemo, Known, QuarantineOutcome};
use crate::host::{Host, OtherHost};
use crate::iso::{HeldIso, WimArch, WimImage};
use crate::test_support::fixtures::{MemoryProgress, non_unicode_name, unicode_home};
use crate::test_support::host::TestHost;
use crate::{Dirs, ErrorKind, Result, Sha256};

fn digest(bytes: &[u8]) -> Sha256 {
    Sha256::from_bytes(Sha2::digest(bytes).into())
}

fn iso_with(home: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = home.join(name);
    std::fs::write(&path, bytes).unwrap();
    crate::host::current().set_mtime(&path, 1_700_000_000).unwrap();
    path
}

fn only_image() -> Vec<WimImage> {
    vec![WimImage {
        index: 1,
        name: "Windows 11 Enterprise Evaluation".to_owned(),
        edition_id: "EnterpriseEval".to_owned(),
        arch: WimArch::X64,
        language: "en-US".to_owned(),
    }]
}

/// The memo of `home`, and how often its images were read from an ISO.
struct Fixture {
    memo: IsoMemo,
    progress: MemoryProgress,
    reads: Cell<usize>,
}

impl Fixture {
    fn new(home: &Path) -> Fixture {
        Fixture {
            memo: IsoMemo::new(&Dirs::under(home)),
            progress: MemoryProgress::default(),
            reads: Cell::new(0),
        }
    }

    /// The SHA-256 of `iso` through `host`; the handle is let go at once.
    fn sha256_via(&self, host: &dyn Host, iso: &Path) -> Result<Sha256> {
        let read_images = |_: &HeldIso| {
            self.reads.set(self.reads.get() + 1);
            Ok(only_image())
        };
        Ok(self.memo.open(host, iso, &self.progress, &read_images)?.sha256)
    }

    fn sha256(&self, iso: &Path) -> Result<Sha256> {
        self.sha256_via(&TestHost, iso)
    }
}

fn hashings(progress: &MemoryProgress) -> usize {
    progress
        .messages()
        .iter()
        .filter(|message| message.starts_with("hashing ") && message.contains(": 0 of "))
        .count()
}

/// Every file in quarantine, one directory deep, with its name and bytes.
fn quarantined_files(dirs: &Dirs) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let Ok(roots) = std::fs::read_dir(dirs.quarantine()) else {
        return Vec::new();
    };
    roots
        .flat_map(|root| std::fs::read_dir(root.unwrap().path()).unwrap())
        .map(|entry| {
            let path = entry.unwrap().path();
            (path.file_name().unwrap().to_owned(), std::fs::read(&path).unwrap())
        })
        .collect()
}

fn memo_files(dirs: &Dirs) -> Vec<PathBuf> {
    std::fs::read_dir(dirs.iso_memo())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .collect()
}

#[skuld::test]
fn the_hash_is_the_sha256_of_the_content_and_is_reused_while_nothing_changed(#[fixture(unicode_home)] home: &Path) {
    let iso = iso_with(home, "win 😀.iso", b"windows");
    let fx = Fixture::new(home);
    assert_eq!(fx.sha256(&iso).unwrap(), digest(b"windows"));
    assert_eq!((hashings(&fx.progress), fx.reads.get()), (1, 1));
    assert_eq!(fx.sha256(&iso).unwrap(), digest(b"windows"));
    assert_eq!(
        (hashings(&fx.progress), fx.reads.get()),
        (1, 1),
        "the second call read the memo: no hash, and no image list read from the ISO"
    );
    // Another process's memo object reads the same file.
    let other = Fixture::new(home);
    assert_eq!(other.sha256(&iso).unwrap(), digest(b"windows"));
    assert_eq!((hashings(&other.progress), other.reads.get()), (0, 0));
}

#[skuld::test]
fn the_memo_gives_back_the_image_list_it_read_with_the_hash(#[fixture(unicode_home)] home: &Path) {
    let iso = iso_with(home, "a.iso", b"windows");
    let fx = Fixture::new(home);
    let first = fx
        .memo
        .open(&TestHost, &iso, &fx.progress, &|_| Ok(only_image()))
        .unwrap();
    assert_eq!(first.images, only_image());
    assert_eq!(first.sha256, digest(b"windows"));
    drop(first);
    let hit = fx
        .memo
        .open(&TestHost, &iso, &fx.progress, &|_| {
            panic!("a hit reads nothing from the ISO")
        })
        .unwrap();
    assert_eq!(hit.images, only_image());
    assert_eq!(hit.sha256, digest(b"windows"));
}

#[skuld::test]
fn an_iso_whose_image_list_cannot_be_read_is_refused_and_not_remembered(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let iso = iso_with(home, "a.iso", b"not an installer");
    let fx = Fixture::new(home);
    let error = fx
        .memo
        .open(&TestHost, &iso, &fx.progress, &|_| {
            Err(crate::Error::new(ErrorKind::Recipe, "not a Windows installer ISO"))
        })
        .err()
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert_eq!(
        hashings(&fx.progress),
        0,
        "the list is read before gigabytes are hashed"
    );
    assert!(!dirs.iso_memo().exists());
}

#[skuld::test]
fn any_one_of_the_three_changing_rehashes(#[fixture(unicode_home)] home: &Path) {
    let fx = Fixture::new(home);
    let iso = iso_with(home, "a.iso", b"first");
    fx.sha256(&iso).unwrap();
    assert_eq!(hashings(&fx.progress), 1);

    // The modification time alone.
    crate::host::current().set_mtime(&iso, 1_700_000_001).unwrap();
    assert_eq!(fx.sha256(&iso).unwrap(), digest(b"first"));
    assert_eq!(hashings(&fx.progress), 2, "mtime");

    // The size alone: same mtime as before, one byte more.
    std::fs::write(&iso, b"first!").unwrap();
    crate::host::current().set_mtime(&iso, 1_700_000_001).unwrap();
    assert_eq!(fx.sha256(&iso).unwrap(), digest(b"first!"));
    assert_eq!(hashings(&fx.progress), 3, "size");

    // The file alone: another file of the same size and time takes the name. The copy exists beside the original, so
    // the two have different ids even where a freed id is reused at once.
    let other = iso_with(home, "b.iso", b"first?");
    crate::host::current().set_mtime(&other, 1_700_000_001).unwrap();
    std::fs::rename(&other, &iso).unwrap();
    assert_eq!(fx.sha256(&iso).unwrap(), digest(b"first?"));
    assert_eq!(hashings(&fx.progress), 4, "file id");

    // A copy with equal bytes is another file: hashed again, and each file keeps its own memo.
    let copy = iso_with(home, "c.iso", b"first?");
    assert_eq!(fx.sha256(&copy).unwrap(), digest(b"first?"));
    assert_eq!(hashings(&fx.progress), 5, "another file");
    fx.sha256(&iso).unwrap();
    fx.sha256(&copy).unwrap();
    assert_eq!(hashings(&fx.progress), 5);
}

#[skuld::test]
fn a_corrupt_memo_is_quarantined_and_rebuilt(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let fx = Fixture::new(home);
    let iso = iso_with(home, "a.iso", b"windows");
    fx.sha256(&iso).unwrap();
    let [file] = memo_files(&dirs).try_into().unwrap();
    std::fs::write(&file, b"{ torn").unwrap();

    assert_eq!(fx.sha256(&iso).unwrap(), digest(b"windows"));
    assert_eq!(hashings(&fx.progress), 2, "the torn memo counts as none");
    assert_eq!(
        quarantined_files(&dirs),
        [(file.file_name().unwrap().to_owned(), b"{ torn".to_vec())]
    );
    assert!(
        fx.progress
            .messages()
            .iter()
            .any(|message| message.contains("unreadable")),
        "{:?}",
        fx.progress.messages()
    );
    fx.sha256(&iso).unwrap();
    assert_eq!(hashings(&fx.progress), 2, "the memo made again is used");
}

#[skuld::test]
fn a_missing_iso_is_not_found_naming_it(#[fixture(unicode_home)] home: &Path) {
    let fx = Fixture::new(home);
    let missing = home.join("no such.iso");
    for error in [
        fx.sha256(&missing).unwrap_err(),
        fx.memo.rehash(&TestHost, &missing, &fx.progress).unwrap_err(),
    ] {
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(error.message().contains("no such.iso"), "{error}");
    }
}

#[skuld::test]
fn the_handle_the_memo_gives_reads_the_file_that_was_hashed(#[fixture(unicode_home)] home: &Path) {
    use std::io::{Read, Seek};
    let fx = Fixture::new(home);
    let iso = iso_with(home, "a.iso", b"windows");
    let hashed = fx
        .memo
        .open(&TestHost, &iso, &fx.progress, &|_| Ok(only_image()))
        .unwrap();
    let mut file = hashed.iso.file();
    file.rewind().unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"windows");
    assert_eq!(hashed.iso.path(), iso);
}

#[skuld::test]
fn one_iso_reached_by_different_spellings_is_hashed_once(#[fixture(unicode_home)] home: &Path) {
    let fx = Fixture::new(home);
    let iso = iso_with(home, "a.iso", b"windows");
    std::fs::create_dir(home.join("sub")).unwrap();
    fx.sha256(&iso).unwrap();
    for spelling in [home.join(".").join("a.iso"), home.join("sub").join("..").join("a.iso")] {
        assert_eq!(fx.sha256(&spelling).unwrap(), digest(b"windows"));
    }
    assert_eq!((hashings(&fx.progress), fx.reads.get()), (1, 1));
    assert_eq!(fx.memo.known().unwrap().len(), 1);
}

#[skuld::test]
fn a_volume_without_file_ids_keys_the_memo_by_the_canonical_path_and_never_shares_a_hash(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = Fixture::new(home);
    // Two ISOs of one size and one time, as two builds of an installer can be, and no file id to tell them apart.
    let one = iso_with(home, "one.iso", b"windows 1");
    let two = iso_with(home, "two.iso", b"windows 2");
    std::fs::create_dir(home.join("sub")).unwrap();
    assert_eq!(fx.sha256_via(&OtherHost, &one).unwrap(), digest(b"windows 1"));
    assert_eq!(fx.sha256_via(&OtherHost, &two).unwrap(), digest(b"windows 2"));
    assert_eq!(hashings(&fx.progress), 2);
    // Spellings of one path share its memo.
    for spelling in [
        home.join(".").join("one.iso"),
        home.join("sub").join("..").join("one.iso"),
    ] {
        assert_eq!(fx.sha256_via(&OtherHost, &spelling).unwrap(), digest(b"windows 1"));
    }
    assert_eq!(hashings(&fx.progress), 2);
}

#[skuld::test]
fn an_iso_replaced_at_its_path_leaves_one_memo_which_is_the_new_content(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let fx = Fixture::new(home);
    let iso = iso_with(home, "a.iso", b"windows");
    fx.sha256(&iso).unwrap();
    let replacement = iso_with(home, "new.iso", b"windows, newer");
    std::fs::rename(&replacement, &iso).unwrap();

    assert_eq!(fx.sha256(&iso).unwrap(), digest(b"windows, newer"));
    assert_eq!(
        fx.memo.known().unwrap(),
        [Known::Iso {
            path: iso,
            sha256: digest(b"windows, newer")
        }]
    );
    assert_eq!(memo_files(&dirs).len(), 1);
}

#[skuld::test]
fn a_moved_iso_is_remembered_at_its_new_path(#[fixture(unicode_home)] home: &Path) {
    let fx = Fixture::new(home);
    let before = iso_with(home, "before.iso", b"windows");
    fx.sha256(&before).unwrap();
    let after = home.join("after.iso");
    std::fs::rename(&before, &after).unwrap();

    assert_eq!(fx.sha256(&after).unwrap(), digest(b"windows"));
    assert_eq!(hashings(&fx.progress), 1, "the move is a hit");
    assert_eq!(
        fx.memo.known().unwrap(),
        [Known::Iso {
            path: after,
            sha256: digest(b"windows")
        }]
    );
}

#[skuld::test]
fn an_iso_with_a_non_unicode_name_is_remembered_by_its_file_id(#[fixture(unicode_home)] home: &Path) {
    let fx = Fixture::new(home);
    let iso = home.join(non_unicode_name());
    std::fs::write(&iso, b"windows").unwrap();

    assert_eq!(fx.sha256(&iso).unwrap(), digest(b"windows"));
    assert_eq!(fx.sha256(&iso).unwrap(), digest(b"windows"));
    assert_eq!(
        (hashings(&fx.progress), fx.reads.get()),
        (1, 1),
        "the second call is a hit"
    );
    let known = fx.memo.known().unwrap();
    let [Known::Iso { path, sha256 }] = &known[..] else {
        panic!("{known:?}");
    };
    assert_eq!(*sha256, digest(b"windows"));
    assert!(path.to_string_lossy().contains('\u{FFFD}'), "{}", path.display());
}

#[skuld::test]
fn a_leftover_temporary_is_swept_by_the_next_write(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let fx = Fixture::new(home);
    let iso = iso_with(home, "a.iso", b"windows");
    fx.sha256(&iso).unwrap();
    let leftover = dirs.iso_memo().join("0123456789abcdef.fedcba9876543210.tmp");
    std::fs::write(&leftover, b"half a memo").unwrap();

    let second = iso_with(home, "b.iso", b"windows too");
    fx.sha256(&second).unwrap();
    assert!(!leftover.exists());
    assert_eq!(memo_files(&dirs).len(), 2);
}

#[skuld::test]
fn a_valid_memo_is_never_quarantined_and_a_vanished_one_is_already_handled(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let fx = Fixture::new(home);
    let iso = iso_with(home, "a.iso", b"windows");
    fx.sha256(&iso).unwrap();
    let [file] = memo_files(&dirs).try_into().unwrap();

    assert!(
        matches!(
            fx.memo.quarantine_unreadable(&file, &fx.progress).unwrap(),
            QuarantineOutcome::Valid { path, sha256 } if path == iso && sha256 == digest(b"windows")
        ),
        "it is a memo"
    );
    assert!(file.exists());
    assert_eq!(quarantined_files(&dirs), []);

    std::fs::remove_file(&file).unwrap();
    assert!(
        matches!(
            fx.memo.quarantine_unreadable(&file, &fx.progress).unwrap(),
            QuarantineOutcome::Gone
        ),
        "another process moved it"
    );

    std::fs::write(&file, b"junk").unwrap();
    let QuarantineOutcome::Moved(moved) = fx.memo.quarantine_unreadable(&file, &fx.progress).unwrap() else {
        panic!("junk is moved");
    };
    assert!(moved.starts_with(dirs.quarantine()), "{}", moved.display());
    assert_eq!(std::fs::read(moved).unwrap(), b"junk");
    assert!(!file.exists());
}

#[skuld::test]
fn the_known_isos_are_listed_by_path_and_an_unreadable_memo_is_listed_with_its_reason(
    #[fixture(unicode_home)] home: &Path,
) {
    let dirs = Dirs::under(home);
    let fx = Fixture::new(home);
    assert_eq!(fx.memo.known().unwrap(), []);
    let b = iso_with(home, "b.iso", b"bb");
    let a = iso_with(home, "a.iso", b"aa");
    fx.sha256(&b).unwrap();
    fx.sha256(&a).unwrap();
    std::fs::write(dirs.iso_memo().join("junk.json"), b"[]").unwrap();
    std::fs::write(dirs.iso_memo().join("note.txt"), b"not a memo").unwrap();

    let known = fx.memo.known().unwrap();
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
        known.iter().any(|known| matches!(
            known,
            Known::Unreadable { file, why } if file.ends_with("junk.json") && why.contains("not a memo")
        )),
        "{known:?}"
    );
}

#[skuld::test]
fn rehash_reads_the_iso_now_and_remembers_nothing(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let fx = Fixture::new(home);
    let iso = iso_with(home, "a.iso", b"windows");
    assert_eq!(
        fx.memo.rehash(&TestHost, &iso, &MemoryProgress::default()).unwrap(),
        digest(b"windows")
    );
    assert!(!dirs.iso_memo().exists());
}
