use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::copy::{Snapshot, snapshot};
use crate::host::Host;
use crate::recipe::hash::{CopyManifest, EntryContent, KeyContext, LayerKey, RootContent, root_key, step_key};
use crate::recipe::{CopyFrom, Step};
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::host::TestHost;
use crate::{Arch, ErrorKind, Sha256};

// Git repositories ====================================================================================================

/// Runs `git args` in `dir` with `stdin`, and returns its stdout. Fails the test naming git when it cannot run.
pub(crate) fn git_with(dir: &Path, args: &[&str], stdin: &[u8]) -> String {
    let mut command = Command::new("git");
    TestHost.quiet(&mut command);
    let mut child = command
        .current_dir(dir)
        .args([
            "-c",
            "user.name=devvm",
            "-c",
            "user.email=devvm@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(["-c", "protocol.file.allow=always", "-c", "init.defaultBranch=main"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("the tests need `git` on PATH: {e}"));
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(stdin)
        .expect("git reads its input");
    let out = child.wait_with_output().expect("git finishes");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("git printed UTF-8")
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    git_with(dir, args, b"")
}

/// A new repository `parent/name` that converts nothing and allows long paths.
pub(crate) fn new_repo(parent: &Path, name: &str) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["config", "core.autocrlf", "false"]);
    git(&root, &["config", "core.longpaths", "true"]);
    root
}

pub(crate) fn put(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

pub(crate) fn add_all(root: &Path) {
    git(root, &["add", "-A"]);
}

/// Tracks `link` (a file holding the link text) as a symlink, the way a checkout without symlink support sees one.
fn track_as_symlink(repo: &Path, link: &str) {
    let blob = git(repo, &["hash-object", "-w", link]);
    git(
        repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{},{link}", blob.trim()),
        ],
    );
}

// Snapshots ===========================================================================================================

pub(crate) struct Taken {
    pub snapshot: Snapshot,
    pub notes: Vec<String>,
}

/// `snapshot` of `from` into a fresh directory `scratch/out`.
pub(crate) fn take(from: &Path, read_only: bool, scratch: &Path) -> crate::Result<Taken> {
    let into = scratch.join("out");
    std::fs::create_dir_all(&into).unwrap();
    let progress = MemoryProgress::default();
    let snapshot = snapshot(&TestHost, from, read_only, &into, &progress)?;
    Ok(Taken {
        snapshot,
        notes: progress.messages(),
    })
}

pub(crate) fn paths(snapshot: &Snapshot) -> Vec<&str> {
    snapshot.manifest.entries().iter().map(|e| e.path.as_str()).collect()
}

pub(crate) struct TarEntry {
    pub path: String,
    pub mode: u32,
    pub kind: tar::EntryType,
    pub link: Option<String>,
    pub bytes: Vec<u8>,
}

pub(crate) fn read_tar(archive: &Path) -> Vec<TarEntry> {
    let mut tar = tar::Archive::new(std::fs::File::open(archive).unwrap());
    tar.entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_str().expect("UTF-8 path").to_owned();
            let link = entry
                .link_name()
                .unwrap()
                .map(|l| l.to_str().expect("UTF-8 link").to_owned());
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            TarEntry {
                path,
                mode: entry.header().mode().unwrap(),
                kind: entry.header().entry_type(),
                link,
                bytes,
            }
        })
        .collect()
}

fn sha256(bytes: &[u8]) -> Sha256 {
    use sha2::Digest;
    Sha256::from_bytes(sha2::Sha256::digest(bytes).into())
}

fn modes(taken: &Taken) -> Vec<(String, u32)> {
    read_tar(&taken.snapshot.archive)
        .into_iter()
        .map(|e| (e.path, e.mode))
        .collect()
}

// Tests ===============================================================================================================

#[skuld::test]
fn empty_directories_are_not_tracked_and_not_copied(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "a.txt", b"a");
    std::fs::create_dir_all(repo.join("empty").join("deeper")).unwrap();
    add_all(&repo);
    let taken = take(&repo, false, home).unwrap();
    assert_eq!(paths(&taken.snapshot), ["a.txt"]);
    let entries = read_tar(&taken.snapshot.archive);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "a.txt");
}

#[skuld::test]
fn symlinks_unix_and_windows_index_mode(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "target.txt", b"t");
    // What a checkout without symlink support leaves: a plain file holding the link text, tracked as mode 120000.
    put(&repo, "link", b"target.txt");
    git(&repo, &["add", "target.txt"]);
    track_as_symlink(&repo, "link");
    let taken = take(&repo, false, home).unwrap();
    let link = taken
        .snapshot
        .manifest
        .entries()
        .iter()
        .find(|e| e.path == "link")
        .unwrap();
    assert_eq!(link.mode, 0o120000);
    assert_eq!(link.content, EntryContent::SymlinkTarget("target.txt".into()));
    let entries = read_tar(&taken.snapshot.archive);
    let entry = entries.iter().find(|e| e.path == "link").unwrap();
    assert_eq!(entry.kind, tar::EntryType::Symlink);
    assert_eq!(entry.link.as_deref(), Some("target.txt"));
    assert!(entry.bytes.is_empty());
}

#[skuld::test]
fn a_symlink_target_is_the_text_git_stores_not_what_the_work_tree_holds(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "sub/dir/target.txt", b"t");
    put(&repo, "link", b"sub/dir/target.txt");
    git(&repo, &["add", "sub/dir/target.txt"]);
    track_as_symlink(&repo, "link");
    // What a Windows link (or its stand-in) can read back as: the same path with backslashes.
    put(&repo, "link", b"sub\\dir\\target.txt");
    let taken = take(&repo, false, home).unwrap();
    let link = taken
        .snapshot
        .manifest
        .entries()
        .iter()
        .find(|e| e.path == "link")
        .unwrap();
    assert_eq!(link.content, EntryContent::SymlinkTarget("sub/dir/target.txt".into()));
    let entries = read_tar(&taken.snapshot.archive);
    let entry = entries.iter().find(|e| e.path == "link").unwrap();
    assert_eq!(entry.link.as_deref(), Some("sub/dir/target.txt"));
}

#[skuld::test]
fn a_symlink_inside_a_submodule_takes_its_target_from_the_submodule(#[fixture(unicode_home)] home: &Path) {
    let inner = new_repo(home, "inner");
    put(&inner, "sub/target.txt", b"t");
    put(&inner, "link", b"sub/target.txt");
    git(&inner, &["add", "sub/target.txt"]);
    track_as_symlink(&inner, "link");
    git(&inner, &["commit", "-q", "-m", "x"]);

    let outer = new_repo(home, "outer");
    put(&outer, "top.txt", b"top");
    add_all(&outer);
    git(
        &outer,
        &["submodule", "add", "-q", inner.to_str().unwrap(), "vendor/inner"],
    );
    git(&outer, &["commit", "-q", "-m", "x"]);

    let taken = take(&outer, false, home).unwrap();
    let link = taken
        .snapshot
        .manifest
        .entries()
        .iter()
        .find(|e| e.path == "vendor/inner/link")
        .expect("the submodule's symlink is copied");
    assert_eq!(link.content, EntryContent::SymlinkTarget("sub/target.txt".into()));
}

#[skuld::test]
fn names_with_spaces_non_ascii_quotes_and_a_leading_dash(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    let long = format!("long/{}", "n".repeat(101));
    let names = [
        "a b.txt",
        "\u{e9} \u{416} \u{4e16} \u{1f600}.txt",
        // Windows file names cannot hold `"`, `|` and the like; copy_unix_tests.rs covers those.
        "it's `x` $HOME;&.txt",
        "-dash.txt",
        "--flag",
        "dir with space/sub/f",
        long.as_str(),
    ];
    for (i, name) in names.iter().enumerate() {
        put(&repo, name, format!("content {i}").as_bytes());
    }
    add_all(&repo);
    let taken = take(&repo, false, home).unwrap();
    let mut expected: Vec<&str> = names.to_vec();
    expected.sort_by_key(|name| name.as_bytes());
    assert_eq!(paths(&taken.snapshot), expected);
    let entries = read_tar(&taken.snapshot.archive);
    assert_eq!(entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), expected);
    for entry in &entries {
        let i = names.iter().position(|name| *name == entry.path).unwrap();
        assert_eq!(entry.bytes, format!("content {i}").as_bytes(), "{}", entry.path);
    }
}

#[skuld::test]
fn a_tracked_file_deleted_on_disk_is_skipped_and_a_staged_rename_copies_only_the_new_path(
    #[fixture(unicode_home)] home: &Path,
) {
    let repo = new_repo(home, "repo");
    put(&repo, "keep.txt", b"k");
    put(&repo, "gone.txt", b"g");
    put(&repo, "old.txt", b"o");
    put(&repo, "dir/inner.txt", b"i");
    add_all(&repo);
    git(&repo, &["commit", "-q", "-m", "x"]);
    std::fs::remove_file(repo.join("gone.txt")).unwrap();
    git(&repo, &["mv", "old.txt", "new.txt"]);
    // `dir/inner.txt` is now below a path that is a file, which is also "gone".
    std::fs::remove_dir_all(repo.join("dir")).unwrap();
    put(&repo, "dir", b"a file now");
    let taken = take(&repo, false, home).unwrap();
    assert_eq!(paths(&taken.snapshot), ["keep.txt", "new.txt"]);
}

#[skuld::test]
fn gitignored_untracked_files_are_absent_and_tracked_but_ignored_files_are_present(
    #[fixture(unicode_home)] home: &Path,
) {
    let repo = new_repo(home, "repo");
    put(&repo, ".gitignore", b"*.log\ntarget/\n");
    put(&repo, "src/main.rs", b"fn main() {}");
    put(&repo, "ignored.log", b"x");
    put(&repo, "target/debug/out", b"x");
    put(&repo, "forced.log", b"tracked despite the rule");
    add_all(&repo);
    git(&repo, &["add", "-f", "forced.log"]);
    let taken = take(&repo, false, home).unwrap();
    assert_eq!(paths(&taken.snapshot), [".gitignore", "forced.log", "src/main.rs"]);
}

#[skuld::test]
fn untracked_files_never_appear(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "tracked.txt", b"t");
    add_all(&repo);
    put(&repo, "untracked.txt", b"u");
    put(&repo, "newdir/untracked.txt", b"u");
    let taken = take(&repo, false, home).unwrap();
    assert_eq!(paths(&taken.snapshot), ["tracked.txt"]);
    assert_eq!(read_tar(&taken.snapshot.archive).len(), 1);
}

#[skuld::test]
fn modes_come_from_the_index_not_the_file_system(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    git(&repo, &["config", "core.fileMode", "false"]);
    put(&repo, "run.sh", b"#!/bin/sh\n");
    put(&repo, "data.txt", b"d");
    add_all(&repo);
    git(&repo, &["update-index", "--chmod=+x", "run.sh"]);
    let taken = take(&repo, false, home).unwrap();
    let manifest_modes: Vec<(&str, u32)> = taken
        .snapshot
        .manifest
        .entries()
        .iter()
        .map(|e| (e.path.as_str(), e.mode))
        .collect();
    assert_eq!(manifest_modes, [("data.txt", 0o100644), ("run.sh", 0o100755)]);
    assert_eq!(
        modes(&taken),
        [("data.txt".to_owned(), 0o644), ("run.sh".to_owned(), 0o755)]
    );
}

#[skuld::test]
fn read_only_clears_write_bits_on_files_only(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "sub/run.sh", b"#!/bin/sh\n");
    put(&repo, "sub/data.txt", b"d");
    put(&repo, "target.txt", b"t");
    put(&repo, "link", b"target.txt");
    add_all(&repo);
    git(&repo, &["update-index", "--chmod=+x", "sub/run.sh"]);
    track_as_symlink(&repo, "link");

    let writable = take(&repo, false, &home.join("w")).unwrap();
    let read_only = take(&repo, true, &home.join("r")).unwrap();
    assert_eq!(
        modes(&writable),
        [
            ("link".to_owned(), 0o777),
            ("sub/data.txt".to_owned(), 0o644),
            ("sub/run.sh".to_owned(), 0o755),
            ("target.txt".to_owned(), 0o644),
        ]
    );
    assert_eq!(
        modes(&read_only),
        [
            ("link".to_owned(), 0o777),
            ("sub/data.txt".to_owned(), 0o444),
            ("sub/run.sh".to_owned(), 0o555),
            ("target.txt".to_owned(), 0o444),
        ]
    );
    // No directory entries: extraction creates directories itself, writable, so a build can still add files.
    assert!(
        read_tar(&read_only.snapshot.archive)
            .iter()
            .all(|e| e.kind != tar::EntryType::Directory)
    );
    // The manifest keeps git's modes; `read-only` is hashed with the step.
    assert_eq!(writable.snapshot.manifest, read_only.snapshot.manifest);
}

#[skuld::test]
fn submodules_are_included_recursively_and_an_uninitialised_one_is_skipped_with_a_note(
    #[fixture(unicode_home)] home: &Path,
) {
    let inner = new_repo(home, "inner");
    put(&inner, "lib.txt", b"lib");
    add_all(&inner);
    git(&inner, &["commit", "-q", "-m", "x"]);

    let outer = new_repo(home, "outer");
    put(&outer, "top.txt", b"top");
    add_all(&outer);
    git(
        &outer,
        &["submodule", "add", "-q", inner.to_str().unwrap(), "vendor/inner"],
    );
    git(&outer, &["commit", "-q", "-m", "x"]);

    let initialised = take(&outer, false, &home.join("a")).unwrap();
    assert_eq!(
        paths(&initialised.snapshot),
        [".gitmodules", "top.txt", "vendor/inner/lib.txt"]
    );
    assert!(initialised.notes.is_empty(), "{:?}", initialised.notes);

    // A clone leaves its submodules empty.
    let clone = home.join("clone");
    git(home, &["clone", "-q", outer.to_str().unwrap(), clone.to_str().unwrap()]);
    let skipped = take(&clone, false, &home.join("b")).unwrap();
    assert_eq!(paths(&skipped.snapshot), [".gitmodules", "top.txt"]);
    assert_eq!(skipped.notes.len(), 1, "{:?}", skipped.notes);
    assert!(skipped.notes[0].contains("vendor/inner"), "{:?}", skipped.notes);
}

#[skuld::test]
fn crlf_is_copied_as_on_disk_even_with_autocrlf_true(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    git(&repo, &["config", "core.autocrlf", "true"]);
    put(&repo, "crlf.txt", b"a\r\nb\r\n");
    put(&repo, "lf.txt", b"a\nb\n");
    add_all(&repo);
    let taken = take(&repo, false, home).unwrap();
    let entries = read_tar(&taken.snapshot.archive);
    assert_eq!(entries[0].bytes, b"a\r\nb\r\n");
    assert_eq!(entries[1].bytes, b"a\nb\n");
    let hashes: Vec<_> = taken
        .snapshot
        .manifest
        .entries()
        .iter()
        .map(|e| e.content.clone())
        .collect();
    assert_eq!(
        hashes,
        [
            EntryContent::FileSha256(sha256(b"a\r\nb\r\n")),
            EntryContent::FileSha256(sha256(b"a\nb\n"))
        ]
    );
}

fn context() -> KeyContext {
    KeyContext {
        arch: Arch::Amd64,
        disk: None,
        guest_semantics_version: 1,
    }
}

fn key(manifest: &CopyManifest) -> LayerKey {
    let root = root_key(&context(), &RootContent::Sha256(sha256(b"root")), &[0; 32]);
    let step = Step::Copy {
        from: CopyFrom::GitTracked,
        to: "~/proj".into(),
        read_only: false,
    };
    step_key(&context(), &root, &step, Some(manifest)).unwrap()
}

#[skuld::test]
fn manifest_changes_with_a_rename_or_a_chmod_and_not_with_mtime(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "a.txt", b"a");
    put(&repo, "b.sh", b"b");
    add_all(&repo);
    let before = take(&repo, false, &home.join("1")).unwrap();

    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(repo.join("a.txt"))
        .unwrap();
    file.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000))
        .unwrap();
    drop(file);
    let touched = take(&repo, false, &home.join("2")).unwrap();
    assert_eq!(key(&touched.snapshot.manifest), key(&before.snapshot.manifest));
    assert_eq!(
        std::fs::read(&touched.snapshot.archive).unwrap(),
        std::fs::read(&before.snapshot.archive).unwrap(),
        "the archive is reproducible"
    );

    git(&repo, &["update-index", "--chmod=+x", "b.sh"]);
    let chmod = take(&repo, false, &home.join("3")).unwrap();
    assert_ne!(key(&chmod.snapshot.manifest), key(&before.snapshot.manifest));

    git(&repo, &["mv", "a.txt", "c.txt"]);
    let renamed = take(&repo, false, &home.join("4")).unwrap();
    assert_ne!(key(&renamed.snapshot.manifest), key(&chmod.snapshot.manifest));
}

#[skuld::test]
fn archive_bytes_hash_to_the_manifest(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "a.txt", b"a");
    put(
        &repo,
        "dir/\u{416}.bin",
        &(0..=255u8).cycle().take(100_000).collect::<Vec<_>>(),
    );
    put(&repo, "empty", b"");
    add_all(&repo);
    let taken = take(&repo, false, home).unwrap();
    let entries = read_tar(&taken.snapshot.archive);
    assert_eq!(entries.len(), taken.snapshot.manifest.entries().len());
    for (entry, manifest) in entries.iter().zip(taken.snapshot.manifest.entries()) {
        assert_eq!(entry.path, manifest.path, "the tar follows the manifest order");
        assert_eq!(
            manifest.content,
            EntryContent::FileSha256(sha256(&entry.bytes)),
            "{}",
            entry.path
        );
    }
}

#[skuld::test]
fn repository_root_is_used_when_the_recipe_is_in_a_subdirectory(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "root.txt", b"r");
    put(&repo, "ci/recipes/linux.devvm.yaml", b"y");
    put(&repo, "other/x.txt", b"x");
    add_all(&repo);
    let taken = take(&repo.join("ci").join("recipes"), false, home).unwrap();
    assert_eq!(
        paths(&taken.snapshot),
        ["ci/recipes/linux.devvm.yaml", "other/x.txt", "root.txt"]
    );
}

#[skuld::test]
fn not_a_repository_is_a_recipe_error(#[fixture(unicode_home)] home: &Path) {
    let plain = home.join("not a repo");
    std::fs::create_dir_all(&plain).unwrap();
    let error = take(&plain, false, home)
        .err()
        .expect("a plain directory is not a repository");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains(plain.to_str().unwrap()), "{error}");
}

#[skuld::test]
fn a_directory_inside_the_git_directory_is_a_recipe_error(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    let error = take(&repo.join(".git"), false, home)
        .err()
        .expect("`.git` has no work tree");
    assert_eq!(error.kind(), ErrorKind::Recipe);
}

#[skuld::test]
fn unmerged_paths_are_refused_naming_them(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "conflict.txt", b"x");
    let blob = git(&repo, &["hash-object", "-w", "conflict.txt"]);
    let blob = blob.trim();
    let index =
        format!("100644 {blob} 1\tconflict.txt\n100644 {blob} 2\tconflict.txt\n100644 {blob} 3\tconflict.txt\n");
    git_with(&repo, &["update-index", "--index-info"], index.as_bytes());
    let error = take(&repo, false, home)
        .err()
        .expect("a conflicted index cannot be copied");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("conflict.txt"), "{error}");
}

#[skuld::test]
fn a_tracked_file_replaced_by_a_directory_is_refused_naming_it(#[fixture(unicode_home)] home: &Path) {
    let repo = new_repo(home, "repo");
    put(&repo, "thing", b"x");
    add_all(&repo);
    std::fs::remove_file(repo.join("thing")).unwrap();
    put(&repo, "thing/inner", b"y");
    let error = take(&repo, false, home)
        .err()
        .expect("a directory is not the tracked file");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("thing"), "{error}");
}

#[skuld::test]
fn git_runs_without_the_callers_repository_variables() {
    let command = crate::copy::git::command(&TestHost, Path::new("."));
    let removed: Vec<_> = command
        .get_envs()
        .filter(|(_, value)| value.is_none())
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR"] {
        assert!(
            removed.iter().any(|r| r == name),
            "{name} must be removed, got {removed:?}"
        );
    }
}
