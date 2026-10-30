use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256 as Sha2};

use super::{LazyInstall, ManifestEntry, QemuInstall, Source, Verify, manifest_entry, run_version};
use crate::config::Config;
use crate::host::{self, SystemDirs};
use crate::store::fetch::{Fetcher, Response, Resume, Transport};
use crate::test_support::fixtures::{MemoryProgress, UNICODE_DIR, unicode_home};
use crate::{Dirs, ErrorKind, Result, Sha256};

const VERSION: &str = "11.1.2";
const URL: &str = "https://example.test/qemu.zip";

/// Every file a sidecar zip must hold, with a marker as content.
fn files() -> Vec<(String, Vec<u8>)> {
    let suffix = std::env::consts::EXE_SUFFIX;
    let mut files: Vec<(String, Vec<u8>)> = ["qemu-img", "qemu-system-x86_64", "qemu-system-aarch64"]
        .iter()
        .map(|name| (format!("bin/{name}{suffix}"), b"tool".to_vec()))
        .collect();
    files.extend(
        [
            "edk2-x86_64-code.fd",
            "edk2-aarch64-code.fd",
            "edk2-i386-vars.fd",
            "edk2-arm-vars.fd",
        ]
        .iter()
        .map(|name| (format!("share/{name}"), b"firmware".to_vec())),
    );
    files
}

fn zip_of(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in files {
        writer
            .start_file(name.as_str(), zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn sha_of(data: &[u8]) -> Sha256 {
    Sha256::from_bytes(Sha2::digest(data).into())
}

/// Serves `data` for every URL and counts the requests.
struct Counting {
    data: Vec<u8>,
    gets: AtomicUsize,
}

impl Counting {
    fn serving(data: Vec<u8>) -> Arc<Counting> {
        Arc::new(Counting {
            data,
            gets: AtomicUsize::new(0),
        })
    }

    fn gets(&self) -> usize {
        self.gets.load(Ordering::SeqCst)
    }
}

impl Transport for Counting {
    fn get(&self, _url: &str, _resume: Option<Resume>) -> Result<Response> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        Ok(Response {
            status: 200,
            content_range: None,
            content_length: Some(self.data.len() as u64),
            validator: None,
            warnings: Vec::new(),
            body: Box::new(Cursor::new(self.data.clone())),
        })
    }
}

/// The version check the production source runs as a process, answered from a table: every tool prints the pinned
/// version, and the tools it was asked about are recorded.
fn verified(asked: &Arc<Mutex<Vec<PathBuf>>>) -> Verify {
    let asked = asked.clone();
    Arc::new(move |install: &QemuInstall, name: &str| {
        asked.lock().unwrap().push(install.tool(name));
        Ok(format!("QEMU emulator version {VERSION}"))
    })
}

fn prints(version: &'static str) -> Verify {
    Arc::new(move |_, _| Ok(format!("QEMU emulator version {version}")))
}

struct World {
    dirs: Dirs,
    transport: Arc<Counting>,
    asked: Arc<Mutex<Vec<PathBuf>>>,
    progress: Arc<MemoryProgress>,
    fetcher: Arc<Fetcher>,
    entry: ManifestEntry,
    source: Source,
}

fn world(home: &Path, zip: Vec<u8>) -> World {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let verify = verified(&asked);
    world_with(home, zip.clone(), sha_of(&zip), asked, verify)
}

/// A world whose server sends `served` while the manifest pins `pinned`.
fn world_with(home: &Path, served: Vec<u8>, pinned: Sha256, asked: Arc<Mutex<Vec<PathBuf>>>, verify: Verify) -> World {
    let dirs = Dirs::under(home);
    let transport = Counting::serving(served);
    let progress = Arc::new(MemoryProgress::default());
    let fetcher = Arc::new(Fetcher::new(dirs.downloads(), transport.clone(), progress.clone()));
    let entry = ManifestEntry {
        qemu_version: VERSION.to_owned(),
        url: URL.to_owned(),
        sha256: pinned,
    };
    let source = Source::new(
        dirs.clone(),
        Config::default(),
        fetcher.clone(),
        progress.clone(),
        Ok(entry.clone()),
        verify,
    );
    World {
        dirs,
        transport,
        asked,
        progress,
        fetcher,
        entry,
        source,
    }
}

impl World {
    /// A new source over the same cache, transport and manifest entry.
    fn source_with(&self, config: Config, verify: Verify) -> Source {
        Source::new(
            self.dirs.clone(),
            config,
            self.fetcher.clone(),
            self.progress.clone(),
            Ok(self.entry.clone()),
            verify,
        )
    }

    fn source_configured(&self, qemu_path: PathBuf) -> Source {
        self.source_with(
            Config {
                qemu_path: Some(qemu_path),
            },
            verified(&self.asked),
        )
    }

    fn lazy(&self, verify: Verify) -> LazyInstall {
        LazyInstall::from_source(self.source_with(Config::default(), verify))
    }
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(read) => read
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// What is published under the QEMU cache: everything that is not an extraction directory or the lock.
fn visible(dir: &Path) -> Vec<String> {
    entries(dir).into_iter().filter(|name| !name.starts_with('.')).collect()
}

#[skuld::test]
fn manifest_has_the_windows_entry_and_rejects_an_unknown_triple() {
    let entry = manifest_entry("x86_64-pc-windows-msvc").unwrap();
    assert!(entry.url.contains(&entry.qemu_version), "{entry:?}");
    assert!(entry.url.ends_with("x86_64-pc-windows-msvc.zip"), "{entry:?}");

    let error = manifest_entry("riscv64-unknown-plan9").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert!(error.message().contains("riscv64-unknown-plan9"), "{error}");
}

#[skuld::test]
fn resolve_downloads_extracts_and_publishes_once(#[fixture(unicode_home)] home: &Path) {
    let zip = zip_of(&files());
    let world = world(home, zip.clone());

    let installs: Vec<QemuInstall> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| world.source.resolve().unwrap()))
            .collect();
        handles.into_iter().map(|handle| handle.join().unwrap()).collect()
    });

    assert_eq!(world.transport.gets(), 1, "four threads on a fresh cache download once");
    assert!(installs.iter().all(|install| install.bin == installs[0].bin));
    assert!(installs[0].bin.ends_with("bin") && installs[0].share.ends_with("share"));
    assert_eq!(installs[0].version, VERSION);
    assert_eq!(installs[0].zip_sha256, Some(sha_of(&zip)));
    assert!(installs[0].tool("qemu-img").is_file());
    assert!(installs[0].share.join("edk2-x86_64-code.fd").is_file());
    assert_eq!(
        visible(&world.dirs.qemu()).len(),
        1,
        "{:?}",
        entries(&world.dirs.qemu())
    );
    let all = entries(&world.dirs.qemu());
    assert!(!all.iter().any(|name| name.starts_with(".extract-")), "{all:?}");
    assert_eq!(
        world.asked.lock().unwrap().len(),
        2,
        "the archive was extracted and checked once"
    );
}

#[skuld::test]
fn a_second_resolve_does_not_fetch(#[fixture(unicode_home)] home: &Path) {
    let world = world(home, zip_of(&files()));
    let first = world.source.resolve().unwrap();
    let checks = world.asked.lock().unwrap().len();

    let second = world.source.resolve().unwrap();

    assert_eq!(first.bin, second.bin);
    assert_eq!(world.transport.gets(), 1);
    assert_eq!(
        world.asked.lock().unwrap().len(),
        checks,
        "a published install is not extracted or checked again"
    );
}

#[skuld::test]
fn the_extracted_tools_are_asked_for_their_version_before_publication(#[fixture(unicode_home)] home: &Path) {
    let world = world(home, zip_of(&files()));
    let install = world.source.resolve().unwrap();

    let asked = world.asked.lock().unwrap().clone();
    for tool in ["qemu-img", "qemu-system-x86_64"] {
        let name = format!("{tool}{}", std::env::consts::EXE_SUFFIX);
        assert!(
            asked
                .iter()
                .any(|path| path.file_name().is_some_and(|n| n == name.as_str())),
            "{tool} was not asked: {asked:?}"
        );
    }
    assert!(
        asked.iter().all(|path| !path.starts_with(&install.bin)),
        "the tools are checked in the extraction directory, before they are published"
    );
}

#[skuld::test]
fn a_tool_that_prints_another_version_is_refused_and_publishes_nothing(#[fixture(unicode_home)] home: &Path) {
    let good = zip_of(&files());
    let world = world_with(home, good.clone(), sha_of(&good), Arc::default(), prints("9.0.0"));

    let error = world.source.resolve().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Store, "{error}");
    assert!(
        error.message().contains(VERSION) && error.message().contains("9.0.0"),
        "{error}"
    );
    assert!(visible(&world.dirs.qemu()).is_empty());
}

#[skuld::test]
fn an_archive_missing_a_file_is_refused_naming_it(#[fixture(unicode_home)] home: &Path) {
    let mut files = files();
    files.retain(|(name, _)| !name.ends_with("edk2-i386-vars.fd"));
    let world = world(home, zip_of(&files));

    let error = world.source.resolve().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Store, "{error}");
    assert!(error.message().contains("edk2-i386-vars.fd"), "{error}");
    assert!(visible(&world.dirs.qemu()).is_empty());
}

/// Every file called `name` under `dir`, however deep.
fn find(dir: &Path, name: &str, found: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for entry in read {
        let path = entry.unwrap().path();
        if path.is_dir() {
            find(&path, name, found);
        } else if path.file_name().is_some_and(|n| n == name) {
            found.push(path);
        }
    }
}

/// Entry names that point out of the extraction directory by one route or another.
const ESCAPES: &[&str] = &[
    "../escaped.txt",
    "bin/../../escaped.txt",
    "bin/../../../escaped.txt",
    "..\\escaped.txt",
    "bin\\..\\..\\escaped.txt",
    "/escaped.txt",
    "\\escaped.txt",
    "C:\\escaped.txt",
    "C:/escaped.txt",
    "//server/share/escaped.txt",
    "escaped.txt:stream",
    "bin/NUL/escaped.txt",
];

/// An archive holding an entry that points out of the extraction directory is refused, or the entry is unpacked inside
/// the published install: it is never a file anywhere else.
#[skuld::test]
fn a_zip_entry_that_points_out_of_the_extraction_directory_never_lands_outside_the_install(
    #[fixture(unicode_home)] home: &Path,
) {
    for (index, name) in ESCAPES.iter().enumerate() {
        let mut files = files();
        files.push(((*name).to_owned(), b"x".to_vec()));
        let sub = home.join(format!("escape-{index}"));
        std::fs::create_dir_all(&sub).unwrap();
        let world = world(&sub, zip_of(&files));

        let result = world.source.resolve();

        let qemu = world.dirs.qemu();
        let mut found = Vec::new();
        find(&sub, "escaped.txt", &mut found);
        for path in &found {
            // `<qemu>/<published install>/...`: at least the install directory and the file below it.
            let relative = path.strip_prefix(&qemu).ok();
            let inside = relative.is_some_and(|relative| {
                relative.components().count() >= 2
                    && relative
                        .components()
                        .next()
                        .is_some_and(|first| !first.as_os_str().to_string_lossy().starts_with('.'))
            });
            assert!(inside, "{name:?} was unpacked to {}", path.display());
        }
        match result {
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::Store, "{name:?}: {error}");
                assert!(visible(&qemu).is_empty(), "{name:?} left {:?}", entries(&qemu));
            }
            Ok(install) => assert!(install.tool("qemu-img").is_file(), "{name:?}"),
        }
    }
}

#[skuld::test]
fn a_dot_dot_entry_is_refused_naming_the_entry(#[fixture(unicode_home)] home: &Path) {
    for (index, name) in ["../escaped.txt", "bin/../../escaped.txt"].iter().enumerate() {
        let mut files = files();
        files.push(((*name).to_owned(), b"x".to_vec()));
        let sub = home.join(format!("dotdot-{index}"));
        std::fs::create_dir_all(&sub).unwrap();
        let world = world(&sub, zip_of(&files));

        let error = world.source.resolve().unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Store, "{name:?}: {error}");
        assert!(error.message().contains("escaped.txt"), "{name:?}: {error}");
        assert!(visible(&world.dirs.qemu()).is_empty());
    }
}

#[skuld::test]
fn a_symbolic_link_in_the_zip_is_refused(#[fixture(unicode_home)] home: &Path) {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in files() {
        writer
            .start_file(name.as_str(), zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(&bytes).unwrap();
    }
    writer
        .add_symlink("share/link", "../outside", zip::write::SimpleFileOptions::default())
        .unwrap();
    let world = world(home, writer.finish().unwrap().into_inner());

    let error = world.source.resolve().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Store, "{error}");
    assert!(error.message().contains("symbolic link"), "{error}");
    assert!(visible(&world.dirs.qemu()).is_empty());
}

#[skuld::test]
fn a_corrupt_zip_is_a_checksum_error_and_leaves_no_entry(#[fixture(unicode_home)] home: &Path) {
    let good = zip_of(&files());
    let mut bad = good.clone();
    bad[10] ^= 0xff;
    let world = world_with(
        home,
        bad,
        sha_of(&good),
        Arc::default(),
        Arc::new(|_, _| unreachable!("a corrupt archive is never extracted")),
    );

    let error = world.source.resolve().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Checksum, "{error}");
    assert!(visible(&world.dirs.qemu()).is_empty());
}

#[skuld::test]
fn a_stale_extraction_directory_is_removed_by_the_next_resolve(#[fixture(unicode_home)] home: &Path) {
    let world = world(home, zip_of(&files()));
    let stale = world.dirs.qemu().join(".extract-killed");
    std::fs::create_dir_all(stale.join("bin")).unwrap();
    std::fs::write(stale.join("bin").join("half"), b"half").unwrap();

    world.source.resolve().unwrap();

    assert!(!stale.exists(), "{:?}", entries(&world.dirs.qemu()));
}

#[skuld::test]
fn a_killed_extraction_leaves_no_visible_entry_and_the_next_call_succeeds(#[fixture(unicode_home)] home: &Path) {
    let world = world(home, zip_of(&files()));
    // The archive is in the download cache, so the helper never reaches the network.
    world.fetcher.fetch(&world.entry.artifact()).unwrap();

    let mut child = crate::test_support::helper::command(
        "qemu-extract",
        &[
            &home.display().to_string(),
            &world.entry.url,
            &world.entry.sha256.to_string(),
        ],
    )
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .spawn()
    .unwrap();
    // The helper prints a line once the archive is extracted and before it is published, then waits on stdin.
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(child.stdout.take().unwrap()), &mut line).unwrap();
    assert_eq!(line.trim(), "extracted", "{line:?}");
    child.kill().unwrap();
    child.wait().unwrap();

    assert!(
        visible(&world.dirs.qemu()).is_empty(),
        "a killed extraction published something"
    );

    let install = world.source.resolve().unwrap();
    assert!(install.tool("qemu-img").is_file());
    assert!(
        entries(&world.dirs.qemu())
            .iter()
            .all(|name| !name.starts_with(".extract-")),
        "the killed extraction's directory was not removed"
    );
}

/// Helper `qemu-extract <home> <url> <sha256>`: resolves up to the point where the archive is extracted and checked,
/// tells the test, and waits for stdin to close (the test kills it first).
pub(crate) fn extract_and_wait(args: &[String]) {
    struct Unreachable;
    impl Transport for Unreachable {
        fn get(&self, url: &str, _: Option<Resume>) -> Result<Response> {
            panic!("the helper reached the network for {url}")
        }
    }
    let dirs = Dirs::under(Path::new(&args[0]));
    let progress = Arc::new(MemoryProgress::default());
    let fetcher = Arc::new(Fetcher::new(dirs.downloads(), Arc::new(Unreachable), progress.clone()));
    let entry = ManifestEntry {
        qemu_version: VERSION.to_owned(),
        url: args[1].clone(),
        sha256: args[2].parse().unwrap(),
    };
    let verify: Verify = Arc::new(|_, _| {
        println!("extracted");
        let mut sink = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut sink);
        Ok(format!("QEMU emulator version {VERSION}"))
    });
    Source::new(dirs, Config::default(), fetcher, progress, Ok(entry), verify)
        .resolve()
        .unwrap();
}

fn touch(path: PathBuf) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"x").unwrap();
}

fn make_tools(dir: &Path) {
    for tool in ["qemu-img", "qemu-system-x86_64"] {
        touch(dir.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX)));
    }
}

fn make_firmware(dir: &Path) {
    for name in ["edk2-x86_64-code.fd", "edk2-i386-vars.fd"] {
        touch(dir.join(name));
    }
}

#[skuld::test]
fn config_path_with_firmware_beside_or_above_is_accepted_and_without_is_unavailable_naming_both(
    #[fixture(unicode_home)] home: &Path,
) {
    let world = world(home, zip_of(&files()));

    // Firmware in `<dir>/share`.
    let beside = home.join("beside").join("bin");
    make_tools(&beside);
    make_firmware(&beside.join("share"));
    let install = world.source_configured(beside.clone()).resolve().unwrap();
    assert_eq!(install.bin, beside);
    assert_eq!(install.share, beside.join("share"));
    assert_eq!(install.zip_sha256, None);
    assert_eq!(world.transport.gets(), 0, "a configured QEMU downloads nothing");

    // Firmware in `<dir>/../share`.
    let above = home.join("above").join("bin");
    make_tools(&above);
    make_firmware(&home.join("above").join("share"));
    let install = world.source_configured(above.clone()).resolve().unwrap();
    assert_eq!(install.share, home.join("above").join("share"));

    // Firmware nowhere: the error names both places.
    let none = home.join("none").join("bin");
    make_tools(&none);
    let error = world.source_configured(none.clone()).resolve().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(
        error.message().contains(&none.join("share").display().to_string()),
        "{error}"
    );
    assert!(
        error
            .message()
            .contains(&home.join("none").join("share").display().to_string()),
        "{error}"
    );
}

#[skuld::test]
fn a_relative_qemu_path_is_unavailable_naming_the_setting(#[fixture(unicode_home)] home: &Path) {
    let world = world(home, zip_of(&files()));
    let relative = PathBuf::from("tools").join("qemu").join("bin");
    let source = world.source_configured(relative.clone());

    for error in [source.resolve().unwrap_err(), source.existing().unwrap_err()] {
        assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
        assert!(error.message().contains("qemu.path"), "{error}");
        assert!(error.message().contains(&relative.display().to_string()), "{error}");
    }
    assert_eq!(world.transport.gets(), 0);
}

#[skuld::test]
fn a_configured_directory_without_the_tools_is_unavailable_naming_the_tool(#[fixture(unicode_home)] home: &Path) {
    let world = world(home, zip_of(&files()));
    let empty = home.join("empty");
    std::fs::create_dir_all(&empty).unwrap();

    let error = world.source_configured(empty).resolve().unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().contains("qemu-img"), "{error}");
}

fn fake_system_dirs() -> SystemDirs {
    SystemDirs {
        root: PathBuf::from("windows-root"),
        system32: PathBuf::from("windows-root").join("system32"),
    }
}

fn install_at(bin: &str) -> QemuInstall {
    QemuInstall {
        bin: PathBuf::from(bin),
        share: PathBuf::from("share"),
        version: VERSION.to_owned(),
        zip_sha256: None,
    }
}

#[skuld::test]
fn environment_is_exactly_path_and_systemroot_and_path_has_system32_first() {
    let install = install_at("qemu-bin");
    let environment = install.environment(Some(&fake_system_dirs())).unwrap();

    let names: Vec<_> = environment
        .iter()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["SystemRoot", "PATH"]);
    assert_eq!(environment[0].1, PathBuf::from("windows-root").into_os_string());
    let path: Vec<PathBuf> = std::env::split_paths(&environment[1].1).collect();
    assert_eq!(
        path,
        [
            PathBuf::from("windows-root").join("system32"),
            PathBuf::from("qemu-bin")
        ]
    );
}

#[skuld::test]
fn environment_without_system_dirs_is_path_to_bin_only() {
    let environment = install_at("qemu-bin").environment(None).unwrap();

    assert_eq!(environment.len(), 1);
    assert_eq!(environment[0].0, "PATH");
    assert_eq!(environment[0].1, PathBuf::from("qemu-bin").into_os_string());
}

#[skuld::test]
fn tool_appends_the_platform_suffix() {
    let tool = install_at("qemu-bin").tool("qemu-img");
    assert_eq!(
        tool,
        PathBuf::from("qemu-bin").join(format!("qemu-img{}", std::env::consts::EXE_SUFFIX))
    );
}

#[skuld::test]
fn lazy_install_downloads_nothing_until_get_and_existing_never_downloads(#[fixture(unicode_home)] home: &Path) {
    let world = world(home, zip_of(&files()));
    let lazy = world.lazy(verified(&world.asked));

    assert!(lazy.existing().unwrap().is_none(), "nothing is installed yet");
    assert_eq!(world.transport.gets(), 0, "existing never downloads");

    let first = lazy.get().unwrap();
    assert_eq!(world.transport.gets(), 1);
    assert!(lazy.existing().unwrap().is_some());
    let again = lazy.get().unwrap();
    assert!(Arc::ptr_eq(&first, &again), "resolved once");
    assert_eq!(world.transport.gets(), 1);
}

#[skuld::test]
fn four_threads_calling_get_resolve_once(#[fixture(unicode_home)] home: &Path) {
    let world = world(home, zip_of(&files()));
    let lazy = world.lazy(verified(&world.asked));

    let installs: Vec<Arc<QemuInstall>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4).map(|_| scope.spawn(|| lazy.get().unwrap())).collect();
        handles.into_iter().map(|handle| handle.join().unwrap()).collect()
    });

    assert!(installs.iter().all(|install| Arc::ptr_eq(install, &installs[0])));
    assert_eq!(world.transport.gets(), 1);
    assert_eq!(world.asked.lock().unwrap().len(), 2, "extracted and checked once");
}

#[skuld::test]
fn a_failed_get_is_not_cached(#[fixture(unicode_home)] home: &Path) {
    let world = world(home, zip_of(&files()));
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    // The first check fails, every later one passes.
    let lazy = world.lazy(Arc::new(move |_, _| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok("QEMU emulator version 9.0.0".to_owned())
        } else {
            Ok(format!("QEMU emulator version {VERSION}"))
        }
    }));

    assert!(lazy.get().is_err(), "the first resolve fails");
    assert!(lazy.get().is_ok(), "the failure was not cached");
}

#[skuld::test]
fn an_unknown_triple_fails_at_get_naming_it_and_downloads_nothing(#[fixture(unicode_home)] home: &Path) {
    let world = world(home, zip_of(&files()));
    let source = Source::new(
        world.dirs.clone(),
        Config::default(),
        world.fetcher.clone(),
        world.progress.clone(),
        manifest_entry("riscv64-unknown-plan9"),
        verified(&world.asked),
    );
    let lazy = LazyInstall::from_source(source);

    assert_eq!(lazy.get().unwrap_err().kind(), ErrorKind::Unavailable);
    let error = lazy.get().unwrap_err();
    assert!(error.message().contains("riscv64-unknown-plan9"), "{error}");
    assert_eq!(world.transport.gets(), 0);
    assert!(lazy.existing().is_err());
}

#[skuld::test]
fn paths_with_space_and_non_ascii_are_returned_verbatim(#[fixture(unicode_home)] home: &Path) {
    let world = world(home, zip_of(&files()));
    let install = world.source.resolve().unwrap();

    assert!(install.bin.starts_with(home), "{}", install.bin.display());
    let shown = install.tool("qemu-system-x86_64").display().to_string();
    assert!(shown.contains(UNICODE_DIR), "{shown}");
    assert!(install.tool("qemu-system-x86_64").is_file());
}

// The version check ====================================================================================================

/// When this test binary runs as a copy installed as `qemu-img`, prints what it was started with (the version check's
/// environment and working directory) as `--version` output, and exits. Called first in the test binary's `main`.
pub(crate) fn act_as_qemu_img_when_installed_as_one() {
    let exe = std::env::current_exe().unwrap();
    if exe.file_stem().is_none_or(|stem| stem != "qemu-img") {
        return;
    }
    let cwd = std::env::current_dir().unwrap();
    let mut env: Vec<String> = std::env::vars_os()
        // Windows keeps per-drive working directories in variables named `=C:`.
        .filter(|(name, _)| !name.to_string_lossy().starts_with('='))
        .map(|(name, value)| format!("env {}={}", name.to_string_lossy(), value.to_string_lossy()))
        .collect();
    env.sort();
    println!("QEMU emulator version {VERSION}");
    println!("cwd {}", cwd.display());
    println!("entries {}", std::fs::read_dir(&cwd).unwrap().count());
    for line in env {
        println!("{line}");
    }
    std::process::exit(0);
}

#[skuld::test]
fn the_version_check_runs_a_tool_with_exactly_qemus_environment_in_an_empty_directory(
    #[fixture(unicode_home)] home: &Path,
) {
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let install = QemuInstall {
        bin,
        share: home.join("share"),
        version: VERSION.to_owned(),
        zip_sha256: None,
    };
    std::fs::copy(std::env::current_exe().unwrap(), install.tool("qemu-img")).unwrap();

    let printed = run_version(host::current(), &install, "qemu-img").unwrap();

    let mut expected: Vec<String> = install
        .environment(host::current().system_dirs().as_ref())
        .unwrap()
        .iter()
        .map(|(name, value)| format!("env {}={}", name.to_string_lossy(), value.to_string_lossy()))
        .collect();
    expected.sort();
    let env: Vec<&str> = printed.lines().filter(|line| line.starts_with("env ")).collect();
    assert_eq!(env, expected, "{printed}");
    assert!(
        printed.contains("entries 0"),
        "the working directory is not empty: {printed}"
    );
    let cwd = printed.lines().find_map(|line| line.strip_prefix("cwd ")).unwrap();
    assert_ne!(
        Path::new(cwd),
        std::env::current_dir().unwrap(),
        "the caller's directory"
    );
}
