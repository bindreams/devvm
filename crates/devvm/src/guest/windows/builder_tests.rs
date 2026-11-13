//! `WindowsBuilder::build_base` over the fake backend, whose guest is a script: it writes the serial log and the answer
//! disk as a guest does, and powers off, which ends the fake VM process. The tests that launch spawn processes.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use fatfs::{FileSystem, FsOptions};

use super::{Bounds, WindowsBuilder};
use crate::backend::fake::{FakeBackend, GuestScript};
use crate::backend::{MachineSpec, MediaFormat, MediaKind};
use crate::contain::Lifetime;
use crate::guest::windows::pins::VIRTIO_WIN;
use crate::guest::windows::templates;
use crate::guest::{BaseBuilder, BuildCtx};
use crate::iso::{IsoMemo, key_iso};
use crate::media::read_files;
use crate::recipe::Source;
use crate::store::fetch::Fetcher;
use crate::store::layers::layers_tests::{Env, env, key, request};
use crate::store::layers::{Built, ChainLease, Clock};
use crate::test_support::fixtures::{build_ctx, iso_fixture, unicode_home};
use crate::test_support::host::TestHost;
use crate::test_support::transport::RefusingTransport;
use crate::{Arch, Dirs, ErrorKind, Result};

// A license read whose grace is 90 days, as an evaluation Windows prints it.
const LICENSED: &str = r#"DEVVM-LICENSE={"Description":"d","LicenseStatus":1,"GracePeriodRemaining":129600,"LicenseFamily":"EnterpriseEval"}"#;
const BORN_EXPIRED: &str =
    r#"DEVVM-LICENSE={"Description":"d","LicenseStatus":5,"GracePeriodRemaining":0,"LicenseFamily":"EnterpriseEval"}"#;

/// What the guest does to its media before it powers off.
type Before = Box<dyn Fn(&MachineSpec) + Send + Sync>;

/// What a fake guest does in one launch.
struct Launch {
    /// Written to the serial log.
    serial: String,
    /// Done before the guest powers off: it may write the answer disk.
    before: Option<Before>,
    /// The guest powers off at the end; otherwise it runs for ever.
    off: bool,
}

impl Launch {
    fn off(serial: &str) -> Launch {
        Launch {
            serial: serial.to_owned(),
            before: None,
            off: true,
        }
    }

    fn running(serial: &str) -> Launch {
        Launch {
            off: false,
            ..Launch::off(serial)
        }
    }

    fn before(mut self, action: impl Fn(&MachineSpec) + Send + Sync + 'static) -> Launch {
        self.before = Some(Box::new(action));
        self
    }
}

/// A launch that finishes the build: the markers of a good first logon.
fn finished(license: &str) -> Launch {
    Launch::off(&format!(
        "DEVVM-PROGRESS=first logon: x\r\nDEVVM-PROGRESS=installing OpenSSH\r\n{license}\r\nDEVVM-BASE-DONE\r\n"
    ))
}

fn guest(launches: Vec<Launch>) -> GuestScript {
    Arc::new(move |number, spec, power| {
        let launch = launches
            .get(number)
            .unwrap_or_else(|| panic!("the build made launch {} of {}", number + 1, launches.len()));
        std::fs::write(&spec.serial_log, &launch.serial).unwrap();
        if let Some(before) = &launch.before {
            before(spec);
        }
        if launch.off {
            power.power_off();
        }
    })
}

/// Short bounds, so that a stall ends the test; `setup` and `first_logon` are the ones a test expects to expire.
fn bounds(setup: Duration, first_logon: Duration) -> Bounds {
    Bounds {
        setup,
        first_logon,
        poll: Duration::from_millis(20),
    }
}

const HOUR: Duration = Duration::from_secs(3600);

struct Rig {
    env: Env,
    dirs: Dirs,
    backend: Arc<FakeBackend>,
    transport: Arc<RefusingTransport>,
    builder: WindowsBuilder,
    ctx: BuildCtx,
    iso: PathBuf,
}

/// A builder over `backend`, whose ISO is the fixture `iso` copied into `home`, with the pinned virtio-win ISO already
/// in the download cache (any bytes: the cache is trusted by name).
fn rig(home: &Path, iso: &str, backend: FakeBackend, bounds: Bounds) -> Rig {
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
    Rig {
        env,
        dirs,
        backend,
        transport,
        builder,
        ctx,
        iso: path,
    }
}

impl Rig {
    /// The base layer a build of the ISO makes, or its error.
    fn build(&self, edition: Option<&str>) -> Result<ChainLease> {
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
    fn work(&self) -> PathBuf {
        self.ctx.work.join("windows-base")
    }

    fn serial_log(&self, launch: usize) -> PathBuf {
        self.work().join(format!("serial-{launch}.log"))
    }

    fn progress(&self) -> Vec<String> {
        self.env.progress.messages()
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
}

/// What `guest` wrote to `image` at `path`, as Windows Setup writes into the answer disk.
fn guest_writes(image: &Path, path: &str, bytes: &[u8]) {
    let mut disk = Cursor::new(std::fs::read(image).unwrap());
    {
        let fs = FileSystem::new(&mut disk, FsOptions::new()).unwrap();
        {
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
    std::fs::write(image, disk.into_inner()).unwrap();
}

/// The answer disk of a build, whose media is the writable USB disk.
fn answer_disk(spec: &MachineSpec) -> PathBuf {
    let writable: Vec<_> = spec.media.iter().filter(|medium| !medium.readonly).collect();
    assert_eq!(writable.len(), 1, "{:?}", spec.media);
    assert_eq!(writable[0].kind, MediaKind::UsbDisk);
    writable[0].path.clone()
}

fn text_of(path: &Path) -> String {
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

// The launches ========================================================================================================

#[skuld::test]
fn a_relaunch_follows_each_exit_until_the_done_marker(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![
        Launch::off(""),
        Launch::off("DEVVM-PROGRESS=specialize\r\n"),
        finished(LICENSED),
    ]));
    let rig = rig(home, "single-image.iso", backend, Bounds::default());

    rig.build(None).unwrap();

    let specs = rig.backend.specs();
    assert_eq!(specs.len(), 3);
    let kinds = |spec: &MachineSpec| spec.media.iter().map(|medium| medium.kind).collect::<Vec<_>>();
    assert_eq!(
        kinds(&specs[0]),
        [MediaKind::Cdrom, MediaKind::Cdrom, MediaKind::UsbDisk]
    );
    // The install media are not attached again, and the virtio-win CD and the answer disk stay.
    for later in &specs[1..] {
        assert_eq!(kinds(later), [MediaKind::Cdrom, MediaKind::UsbDisk]);
        assert_eq!(later.media[1].path, specs[0].media[2].path);
    }
    assert_eq!(specs[0].media[0].boot_index, Some(1));
    assert_eq!(specs[0].media[0].format, MediaFormat::Qcow2);
    assert!(specs[0].media[0].readonly);
    assert_eq!(specs[1].disk, specs[0].disk);
}

#[skuld::test]
fn every_launch_is_contained_and_without_a_reboot_and_has_its_files_in_the_work_directory(
    #[fixture(unicode_home)] home: &Path,
) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![Launch::off(""), finished(LICENSED)]));
    let rig = rig(home, "single-image.iso", backend, Bounds::default());

    rig.build(None).unwrap();

    for (n, spec) in rig.backend.specs().iter().enumerate() {
        assert_eq!(spec.lifetime, Lifetime::DiesWithOwner);
        assert!(spec.profile.no_reboot, "a guest reboot is a process exit");
        assert!(
            spec.forwards.is_empty(),
            "nothing connects to the guest during the build"
        );
        assert_eq!(spec.serial_log, rig.serial_log(n + 1));
        assert!(spec.dir.starts_with(rig.work()), "{}", spec.dir.display());
        assert_eq!(spec.network, crate::Network::Nat);
        assert_eq!(spec.profile.video, crate::backend::Video::Vga);
        for medium in spec
            .media
            .iter()
            .filter(|medium| medium.kind != MediaKind::Cdrom || medium.boot_index.is_some())
        {
            assert!(medium.path.starts_with(rig.work()), "{}", medium.path.display());
        }
    }
}

#[skuld::test]
fn a_launch_after_the_first_without_a_new_progress_marker_is_a_boot_loop_and_fails_naming_the_launch(
    #[fixture(unicode_home)] home: &Path,
) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![
        Launch::off(""),
        Launch::off("DEVVM-PROGRESS=specialize\r\n"),
        Launch::off("DEVVM-PROGRESS=specialize\r\n"),
    ]));
    let rig = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    let message = error.message();
    assert!(message.contains("launch 3"), "{message}");
    assert!(message.contains("specialize"), "{message}");
    assert!(message.contains(&rig.serial_log(3).display().to_string()), "{message}");
    assert_eq!(rig.backend.launches(), 3, "no fourth launch");
}

#[skuld::test]
fn a_boot_loop_that_never_wrote_a_marker_is_found_at_the_second_launch(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![Launch::off(""), Launch::off("")]));
    let rig = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert!(error.message().contains("launch 2"), "{error}");
    assert_eq!(rig.backend.launches(), 2);
}

#[skuld::test]
fn the_failed_marker_fails_the_build_at_once_and_only_its_prefix_is_read(#[fixture(unicode_home)] home: &Path) {
    let german = "Der Zugriff auf den Pfad wurde verweigert: \u{fc}ber den Server";
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![
        Launch::off(""),
        Launch::off(&format!(
            "DEVVM-PROGRESS=first logon: x\r\nDEVVM-BASE-FAILED={german}\r\nDEVVM-BASE-DONE\r\n"
        )),
    ]));
    let rig = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    let message = error.message();
    assert!(message.contains(german), "{message}");
    assert!(message.contains(&rig.serial_log(2).display().to_string()), "{message}");
    assert_eq!(rig.backend.launches(), 2, "a failed first logon is not a Setup reboot");
}

#[skuld::test]
fn a_missing_license_marker_fails_the_build(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![Launch::off(
        "DEVVM-PROGRESS=first logon: x\r\nDEVVM-BASE-DONE\r\n",
    )]));
    let rig = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().contains("DEVVM-LICENSE"), "{error}");
    assert!(
        error.message().contains(&rig.serial_log(1).display().to_string()),
        "{error}"
    );
}

#[skuld::test]
fn an_unreadable_license_marker_fails_the_build(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![finished("DEVVM-LICENSE=not json")]));
    let rig = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert!(error.message().contains("DEVVM-LICENSE"), "{error}");
}

#[skuld::test]
fn the_license_marker_becomes_layer_meta_expires(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![finished(LICENSED)]));
    // The single image of this ISO is an evaluation edition.
    let rig = rig(home, "single-image.iso", backend, Bounds::default());

    let layer = rig.build(None).unwrap();

    let now: SystemTime = rig.env.clock.now();
    assert_eq!(
        layer.tip().meta().expires,
        Some(now + Duration::from_secs(129_600 * 60))
    );
}

#[skuld::test]
fn a_guest_whose_evaluation_is_not_licensed_ends_the_build_as_expired_base(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![finished(BORN_EXPIRED)]));
    let rig = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ExpiredBase, "{error}");
}

#[skuld::test]
fn a_base_with_less_than_ten_days_of_grace_is_expired_base_naming_the_date(#[fixture(unicode_home)] home: &Path) {
    let nine_days = LICENSED.replace("129600", &(9 * 24 * 60).to_string());
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![finished(&nine_days)]));
    let rig = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ExpiredBase, "{error}");
    assert!(error.message().contains("within 10 days"), "{error}");
}

#[skuld::test]
fn each_launch_writes_its_own_serial_log_and_an_error_names_the_phase_that_failed(
    #[fixture(unicode_home)] home: &Path,
) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![
        Launch::off("first"),
        Launch::off("DEVVM-PROGRESS=specialize\r\nsecond"),
        Launch::off("DEVVM-PROGRESS=first logon: x\r\nDEVVM-BASE-FAILED=boom\r\n"),
    ]));
    let rig = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    for (n, text) in [(1, "first"), (2, "second")] {
        assert!(text_of(&rig.serial_log(n)).contains(text), "serial-{n}.log");
    }
    assert!(
        error.message().contains(&rig.serial_log(3).display().to_string()),
        "{error}"
    );
    assert!(
        !error.message().contains("serial-2.log"),
        "only the failed phase's log: {error}"
    );
}

#[skuld::test]
fn progress_markers_are_echoed_as_progress_lines(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![
        Launch::off(""),
        Launch::off("DEVVM-PROGRESS=specialize\r\n"),
        finished(LICENSED),
    ]));
    let rig = rig(home, "single-image.iso", backend, Bounds::default());

    rig.build(None).unwrap();

    let lines = rig.progress();
    for phase in ["specialize", "first logon: x", "installing OpenSSH"] {
        let echoed = lines.iter().filter(|line| line.contains(phase)).count();
        assert_eq!(echoed, 1, "{phase} once in {lines:?}");
    }
}

#[skuld::test]
fn the_sizes_of_the_logs_winpe_left_on_the_answer_disk_are_reported_at_the_end_of_the_build(
    #[fixture(unicode_home)] home: &Path,
) {
    let winpe = Launch::off("").before(|spec| {
        guest_writes(&answer_disk(spec), "logs/winpe.log", &[b'x'; 1000]);
        guest_writes(&answer_disk(spec), "logs/C-setupact.log", &[b'y'; 234]);
    });
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![winpe, finished(LICENSED)]));
    let rig = rig(home, "single-image.iso", backend, Bounds::default());

    rig.build(None).unwrap();

    let lines = rig.progress();
    let reported: Vec<_> = lines.iter().filter(|line| line.contains("WinPE logs")).collect();
    assert_eq!(
        reported,
        ["Windows base: WinPE logs use 1234 of 8388608 bytes of the answer disk"],
        "{lines:?}"
    );
}

// The failure bounds ==================================================================================================

#[skuld::test]
fn a_stall_in_winpe_names_the_log_read_back_from_the_answer_disk(#[fixture(unicode_home)] home: &Path) {
    let winpe = Launch::running("").before(|spec| {
        guest_writes(
            &answer_disk(spec),
            "logs/winpe.log",
            b"DEVVM-WINPE-FAILED=diskpart\r\nlast winpe line\r\n",
        );
    });
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![winpe]));
    let rig = rig(
        home,
        "single-image.iso",
        backend,
        bounds(Duration::from_millis(300), HOUR),
    );

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    let message = error.message();
    assert!(message.contains("Windows Setup"), "{message}");
    assert!(message.contains(&rig.serial_log(1).display().to_string()), "{message}");
    assert!(message.contains("logs/winpe.log"), "{message}");
    assert!(message.contains("last winpe line"), "{message}");
    assert_eq!(rig.backend.launches(), 1);
}

#[skuld::test]
fn a_stall_after_setup_names_the_first_logon_script_and_not_winpes_logs(#[fixture(unicode_home)] home: &Path) {
    let logon = Launch::running("DEVVM-PROGRESS=first logon: x\r\n").before(|spec| {
        guest_writes(&answer_disk(spec), "logs/winpe.log", b"old winpe line");
    });
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![logon]));
    let rig = rig(
        home,
        "single-image.iso",
        backend,
        bounds(HOUR, Duration::from_millis(300)),
    );

    let error = rig.build(None).unwrap_err();

    let message = error.message();
    assert!(message.contains("the first-logon script"), "{message}");
    assert!(!message.contains("old winpe line"), "WinPE is long over: {message}");
}

#[skuld::test]
fn a_stalled_guest_is_stopped_and_its_files_are_removed(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![Launch::running("")]));
    let rig = rig(
        home,
        "single-image.iso",
        backend,
        bounds(Duration::from_millis(200), HOUR),
    );

    rig.build(None).unwrap_err();

    let launched = rig.backend.launched();
    assert_eq!(launched.len(), 1);
    assert!(
        crate::contain::VmProcess::attach(&launched[0].process)
            .unwrap()
            .is_none(),
        "the VM process still runs"
    );
}

// The ISO's hold ======================================================================================================

#[skuld::test]
fn the_iso_stays_held_until_qemu_has_opened_it_and_is_free_after(#[fixture(unicode_home)] home: &Path) {
    let writable = Arc::new(Mutex::new(Vec::new()));
    let seen = writable.clone();
    let iso = home.join("windows.iso");
    let probe: GuestScript = {
        let script = guest(vec![
            Launch::off(""),
            Launch::off("DEVVM-PROGRESS=specialize\r\n"),
            finished(LICENSED),
        ]);
        Arc::new(move |number, spec, power| {
            // Called as the machine's `resume` runs: before it returns, QEMU has not reported the ISO opened.
            let free = std::fs::OpenOptions::new().write(true).open(&iso).is_ok();
            seen.lock().unwrap().push(free);
            script(number, spec, power)
        })
    };
    let rig = rig(
        home,
        "single-image.iso",
        FakeBackend::new(&TestHost).with_guest(probe),
        Bounds::default(),
    );

    rig.build(None).unwrap();

    assert_eq!(*writable.lock().unwrap(), [false, true, true]);
    assert!(
        std::fs::OpenOptions::new().write(true).open(&rig.iso).is_ok(),
        "the hold is gone with the build"
    );
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
