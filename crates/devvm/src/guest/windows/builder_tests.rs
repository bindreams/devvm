//! `WindowsBuilder::build_base` over the fake backend, for what the build makes before its first launch. The tests with
//! machines that launch need a frozen spawn, which only Windows has, and run in `builder_windows_tests.rs`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{Bounds, WindowsBuilder};
use crate::backend::fake::FakeBackend;
use crate::guest::windows::pins::VIRTIO_WIN;
use crate::guest::windows::templates;
use crate::guest::{BaseBuilder, BuildCtx};
use crate::iso::{IsoMemo, key_iso};
use crate::media::read_files;
use crate::recipe::Source;
use crate::store::fetch::Fetcher;
use crate::store::layers::layers_tests::{Env, env, key, request};
use crate::store::layers::{Built, ChainLease};
use crate::test_support::fixtures::{build_ctx, iso_fixture, unicode_home};
use crate::test_support::host::TestHost;
use crate::test_support::transport::RefusingTransport;
use crate::{Arch, Dirs, ErrorKind, Result};

pub(super) struct Rig {
    pub(super) env: Env,
    pub(super) dirs: Dirs,
    pub(super) transport: Arc<RefusingTransport>,
    pub(super) builder: WindowsBuilder,
    pub(super) ctx: BuildCtx,
    pub(super) iso: PathBuf,
}

/// A builder over `backend`, whose ISO is the fixture `iso` copied into `home`, with the pinned virtio-win ISO already
/// in the download cache (any bytes: the cache is trusted by name). The backend is returned for the tests that read what
/// it launched.
pub(super) fn rig(home: &Path, iso: &str, backend: FakeBackend, bounds: Bounds) -> (Rig, Arc<FakeBackend>) {
    let env = env(home);
    let dirs = Dirs::under(home);
    let transport = Arc::new(RefusingTransport::default());
    let fetcher = Arc::new(Fetcher::new(dirs.downloads(), transport.clone(), env.progress.clone()));
    let cached = fetcher.entry_path(&VIRTIO_WIN.artifact());
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
    std::fs::write(&cached, b"virtio-win").unwrap();
    let backend = Arc::new(backend);
    let builder = WindowsBuilder::new(fetcher, backend.clone(), &TestHost, bounds);
    let mut ctx = build_ctx(home, Arch::Amd64);
    ctx.progress = env.progress.clone();
    ctx.clock = env.clock.clone();
    let path = home.join("windows.iso");
    std::fs::copy(iso_fixture(iso), &path).unwrap();
    let rig = Rig {
        env,
        dirs,
        transport,
        builder,
        ctx,
        iso: path,
    };
    (rig, backend)
}

impl Rig {
    /// The base layer a build of the ISO makes, or its error.
    pub(super) fn build(&self, edition: Option<&str>) -> Result<ChainLease> {
        let root = key_iso(
            &TestHost,
            &IsoMemo::new(&self.dirs),
            &self.iso,
            edition,
            &*self.env.progress,
        )
        .unwrap();
        *self.ctx.iso.lock().unwrap() = Some(root);
        let source = Source::Iso {
            path: self.iso.clone(),
            edition: edition.map(Into::into),
        };
        self.env.store.build(&request(key(1)), None, |staging| {
            self.builder
                .build_base(&source, &self.ctx, staging.dir)
                .map(|meta| Built::from_meta(&meta))
        })
    }

    /// The build's own directory for the base, which nothing but the build removes.
    pub(super) fn work(&self) -> PathBuf {
        self.ctx.work.join("windows-base")
    }
}

/// A rig whose launches never happen, for what the build makes before the first.
fn rig_without_a_machine(home: &Path, iso: &str) -> Rig {
    rig(
        home,
        iso,
        FakeBackend::new(&TestHost).with_failing_launch(),
        Bounds::default(),
    )
    .0
}

pub(super) fn text_of(path: &Path) -> String {
    String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned()
}

// The install media ===================================================================================================

#[skuld::test]
fn boot_patches_become_overlay_patches_with_the_same_offsets_and_bytes(#[fixture(unicode_home)] home: &Path) {
    let rig = rig_without_a_machine(home, "single-image.iso");
    let held = key_iso(&TestHost, &IsoMemo::new(&rig.dirs), &rig.iso, None, &*rig.env.progress).unwrap();
    let expected: Vec<String> = crate::iso::read_install_iso(held)
        .unwrap()
        .boot_patches
        .iter()
        .map(|patch| format!("{}+{}", patch.offset, patch.bytes.len()))
        .collect();
    assert!(!expected.is_empty());

    rig.build(None).unwrap_err();

    let overlay = text_of(&rig.work().join("install.qcow2"));
    assert!(
        overlay.ends_with(&format!("patches={}", expected.join(","))),
        "{overlay}"
    );
}

#[skuld::test]
fn the_overlay_is_made_in_the_builds_work_directory_and_nothing_sweeps_it(#[fixture(unicode_home)] home: &Path) {
    let rig = rig_without_a_machine(home, "single-image.iso");

    let error = rig.build(None).unwrap_err();

    assert!(error.message().contains("cannot launch"), "{error}");
    assert!(
        rig.work().join("install.qcow2").is_file(),
        "the overlay is the build directory's to remove"
    );
    let overlays: Vec<_> = walk(home)
        .into_iter()
        .filter(|path| path.ends_with("install.qcow2"))
        .collect();
    assert_eq!(
        overlays,
        [rig.work().join("install.qcow2")],
        "the overlay is in no layer"
    );
}

/// Every file below `dir`.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}

#[skuld::test]
fn the_blank_disk_is_the_larger_of_64_gib_and_the_recipes_disk(#[fixture(unicode_home)] home: &Path) {
    for (disk, expected) in [
        (None, 64u64 << 30),
        (Some(10u64 << 30), 64 << 30),
        (Some(100u64 << 30), 100 << 30),
    ] {
        let mut rig = rig_without_a_machine(home, "single-image.iso");
        rig.ctx.resources.disk = disk;
        let disks = Mutex::new(Vec::new());

        let error = rig
            .env
            .store
            .build(&request(key(2)), None, |staging| {
                let root = key_iso(&TestHost, &IsoMemo::new(&rig.dirs), &rig.iso, None, &*rig.env.progress)?;
                *rig.ctx.iso.lock().unwrap() = Some(root);
                let source = Source::Iso {
                    path: rig.iso.clone(),
                    edition: None,
                };
                let result = rig.builder.build_base(&source, &rig.ctx, staging.dir);
                disks
                    .lock()
                    .unwrap()
                    .push(text_of(&staging.dir.path().join("disk.qcow2")));
                result.map(|meta| Built::from_meta(&meta))
            })
            .unwrap_err();

        assert!(error.message().contains("cannot launch"), "{error}");
        assert_eq!(*disks.lock().unwrap(), [format!("blank size={expected}")], "{disk:?}");
        let _ = std::fs::remove_dir_all(rig.work());
    }
}

#[skuld::test]
fn the_answer_file_for_each_image_of_the_owners_iso_selects_that_image_by_index(#[fixture(unicode_home)] home: &Path) {
    for (edition, index) in [
        ("Windows 11 Enterprise LTSC 2024", 1),
        ("Windows 11 IoT Enterprise LTSC 2024", 2),
        ("Windows 11 IoT Enterprise Subscription LTSC 2024", 3),
    ] {
        let rig = rig_without_a_machine(home, "three-images.iso");

        rig.build(Some(edition)).unwrap_err();

        let files = read_files(&rig.work().join("answer.img"), &["Autounattend.xml"]).unwrap();
        let answer = String::from_utf8(files[0].bytes.clone()).unwrap();
        assert!(
            answer.contains(&format!(
                "<Key>/IMAGE/INDEX</Key>\n              <Value>{index}</Value>"
            )),
            "{edition}: {answer}"
        );
        assert!(answer.contains("<UILanguage>en-US</UILanguage>"), "{edition}");
        let _ = std::fs::remove_dir_all(rig.work());
    }
}

#[skuld::test]
fn the_virtio_win_iso_is_the_pinned_download_and_a_cached_one_is_not_fetched_again(
    #[fixture(unicode_home)] home: &Path,
) {
    let rig = rig_without_a_machine(home, "single-image.iso");
    let cached = rig.builder.fetcher.entry_path(&VIRTIO_WIN.artifact());
    std::fs::remove_file(&cached).unwrap();

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Download, "{error}");
    assert_eq!(rig.transport.asked(), [VIRTIO_WIN.url]);
}

#[skuld::test]
fn a_base_is_built_from_an_iso_and_only_from_one(#[fixture(unicode_home)] home: &Path) {
    let rig = rig_without_a_machine(home, "single-image.iso");
    let source = Source::Embedded("ubuntu-24.04".to_owned());

    let error = rig
        .env
        .store
        .build(&request(key(3)), None, |staging| {
            rig.builder
                .build_base(&source, &rig.ctx, staging.dir)
                .map(|meta| Built::from_meta(&meta))
        })
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::NotSupported, "{error}");
}

#[skuld::test]
fn the_definition_hash_is_the_templates() {
    let rig_hash = {
        let fetcher = Arc::new(Fetcher::new(
            std::env::temp_dir(),
            Arc::new(RefusingTransport::default()),
            Arc::new(crate::exit::NullProgress),
        ));
        WindowsBuilder::new(
            fetcher,
            Arc::new(FakeBackend::new(&TestHost)),
            &TestHost,
            Bounds::default(),
        )
        .definition_hash()
    };
    assert_eq!(rig_hash, templates::definition_hash());
}

#[skuld::test]
fn the_iso_is_free_again_when_the_build_fails_before_its_first_launch(#[fixture(unicode_home)] home: &Path) {
    let rig = rig_without_a_machine(home, "single-image.iso");

    rig.build(None).unwrap_err();

    assert!(std::fs::OpenOptions::new().write(true).open(&rig.iso).is_ok());
}

#[skuld::test]
fn the_bootstrap_writes_the_marker_that_starts_the_first_logon_phase() {
    let bootstrap = templates::TEMPLATES
        .iter()
        .find(|(name, _)| *name == "bootstrap.ps1")
        .map(|(_, text)| *text)
        .unwrap();
    assert!(
        bootstrap.contains(&format!("DEVVM-PROGRESS={}", super::FIRST_LOGON)),
        "the builder tells the first-logon phase by this phase name"
    );
}

#[skuld::test]
fn a_failing_exit_quotes_what_qemu_printed_or_says_it_printed_nothing() {
    let exited = || {
        crate::Error::new(
            ErrorKind::Unavailable,
            "the VM process of launch 2 exited with exit code: 1",
        )
    };

    let quiet = super::with_qemu_output(exited(), "");
    assert_eq!(
        quiet.message(),
        "the VM process of launch 2 exited with exit code: 1\nQEMU printed nothing"
    );
    assert_eq!(quiet.kind(), ErrorKind::Unavailable);

    let loud = super::with_qemu_output(exited(), "qemu: out of memory");
    assert_eq!(
        loud.message(),
        "the VM process of launch 2 exited with exit code: 1\nthe end of QEMU's output:\nqemu: out of memory"
    );
}
