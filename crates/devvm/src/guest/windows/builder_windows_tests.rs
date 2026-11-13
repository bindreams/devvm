//! `WindowsBuilder::build_base` over the fake backend, with machines that launch: a fake machine is a frozen process, which only
//! Windows can spawn. The guest is a script: it writes the serial log and the answer disk as a guest does, and powers
//! off, which ends the fake VM process.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use fatfs::{FileSystem, FsOptions};

use super::Bounds;
use super::builder_tests::{Rig, rig, text_of};
use crate::backend::fake::{EXIT_OUTPUT, FakeBackend, GuestScript};
use crate::backend::{MachineSpec, MediaFormat, MediaKind};
use crate::contain::Lifetime;
use crate::store::layers::Clock;
use crate::store::layers::layers_tests::{days, t0};
use crate::test_support::fixtures::{FakeClock, unicode_home};
use crate::test_support::host::TestHost;
use crate::{Error, ErrorKind};

impl Rig {
    fn serial_log(&self, launch: usize) -> PathBuf {
        self.work().join(format!("serial-{launch}.log"))
    }

    fn progress(&self) -> Vec<String> {
        self.env.progress.messages()
    }

    /// A failed build's directory goes with it, so its error names nothing in it and carries what it held.
    fn assert_names_no_build_file(&self, error: &Error) {
        assert!(
            !error.message().contains(&self.ctx.work.display().to_string()),
            "{error}"
        );
    }
}

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
    end: End,
}

/// How a launch ends.
enum End {
    /// The guest powers off: the VM process exits with status 0.
    PowersOff,
    /// The VM process dies with status 1.
    Crashes,
    /// The guest runs for ever.
    Runs,
}

impl Launch {
    fn off(serial: &str) -> Launch {
        Launch {
            serial: serial.to_owned(),
            before: None,
            end: End::PowersOff,
        }
    }

    fn running(serial: &str) -> Launch {
        Launch {
            end: End::Runs,
            ..Launch::off(serial)
        }
    }

    fn crashing(serial: &str) -> Launch {
        Launch {
            end: End::Crashes,
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
        match launch.end {
            End::PowersOff => power.power_off(),
            End::Crashes => power.crash(),
            End::Runs => {}
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

// The launches ========================================================================================================

#[skuld::test]
fn a_relaunch_follows_each_exit_until_the_done_marker(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![
        Launch::off(""),
        Launch::off("DEVVM-PROGRESS=specialize\r\n"),
        finished(LICENSED),
    ]));
    let (rig, backend) = rig(home, "single-image.iso", backend, Bounds::default());

    rig.build(None).unwrap();

    let specs = backend.specs();
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
    let (rig, backend) = rig(home, "single-image.iso", backend, Bounds::default());

    rig.build(None).unwrap();

    for (n, spec) in backend.specs().iter().enumerate() {
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
    let (rig, backend) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    let message = error.message();
    assert!(message.contains("launch 3"), "{message}");
    assert!(message.contains("specialize"), "{message}");
    assert!(message.contains("the end of the serial log"), "{message}");
    rig.assert_names_no_build_file(&error);
    assert_eq!(backend.launches(), 3, "no fourth launch");
}

#[skuld::test]
fn a_boot_loop_that_never_wrote_a_marker_is_found_at_the_second_launch(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![Launch::off(""), Launch::off("")]));
    let (rig, backend) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert!(error.message().contains("launch 2"), "{error}");
    rig.assert_names_no_build_file(&error);
    assert_eq!(backend.launches(), 2);
}

#[skuld::test]
fn an_exit_with_a_failure_status_is_an_error_naming_the_launch_and_status(#[fixture(unicode_home)] home: &Path) {
    let crashed = Launch::crashing("DEVVM-PROGRESS=specialize\r\n");
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![Launch::off(""), crashed]));
    let (rig, backend) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    let message = error.message();
    assert!(message.contains("launch 2"), "{message}");
    assert!(message.contains("exit code: 1"), "{message}");
    assert!(message.contains("specialize"), "the serial tail: {message}");
    assert!(
        message.contains(EXIT_OUTPUT),
        "the machine's own account of the exit: {message}"
    );
    rig.assert_names_no_build_file(&error);
    assert_eq!(backend.launches(), 2, "a crashed VM is not a Setup reboot");
}

#[skuld::test]
fn a_wait_that_fails_still_reads_the_markers_the_guest_wrote_before_it_names_the_error(
    #[fixture(unicode_home)] home: &Path,
) {
    let logon = Launch::running("DEVVM-PROGRESS=first logon: x\r\nlast serial line\r\n").before(|spec| {
        guest_writes(&answer_disk(spec), "logs/winpe.log", b"old winpe line");
    });
    let backend = FakeBackend::new(&TestHost)
        .with_guest(guest(vec![logon]))
        .with_failing_try_wait();
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    let message = error.message();
    assert!(message.contains("cannot read the VM process's status"), "{message}");
    assert!(message.contains("last serial line"), "the serial tail: {message}");
    assert!(
        rig.progress().iter().any(|line| line.contains("first logon: x")),
        "the marker the wait never polled is reported: {:?}",
        rig.progress()
    );
    assert!(
        !message.contains("old winpe line"),
        "a marker was seen, so WinPE's logs are not the account of the failure: {message}"
    );
    rig.assert_names_no_build_file(&error);
}

#[skuld::test]
fn a_vm_that_could_not_be_killed_may_still_write_so_only_its_complete_lines_are_read(
    #[fixture(unicode_home)] home: &Path,
) {
    let logon = Launch::off("DEVVM-PROGRESS=first logon: whole\nDEVVM-PROGRESS=first logon: half");
    let backend = FakeBackend::new(&TestHost)
        .with_guest(guest(vec![logon]))
        .with_failing_kill();
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert!(error.message().contains("the fake machine's kill fails"), "{error}");
    let progress = rig.progress().join("\n");
    assert!(progress.contains("first logon: whole"), "{progress}");
    assert!(
        !progress.contains("first logon: half"),
        "a line the VM may still be writing is not a marker: {progress}"
    );
}

#[skuld::test]
fn a_wait_and_a_kill_that_both_fail_are_both_named(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost)
        .with_guest(guest(vec![Launch::running("DEVVM-PROGRESS=first logon: x\n")]))
        .with_failing_try_wait()
        .with_failing_kill();
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    let message = error.message();
    assert!(message.contains("cannot read the VM process's status"), "{message}");
    assert!(message.contains("the fake machine's kill fails"), "{message}");
    assert!(
        message.contains("may still run"),
        "the VM may still be running: {message}"
    );
}

#[skuld::test]
fn a_machine_that_fails_to_resume_is_killed_and_a_kill_that_fails_too_is_named(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_failing_resume().with_failing_kill();
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    let message = error.message();
    assert!(message.contains("the fake machine's resume fails"), "{message}");
    assert!(message.contains("the fake machine's kill fails"), "{message}");
    assert!(message.contains("may still run"), "{message}");
}

#[skuld::test]
fn a_failed_marker_is_the_headline_whatever_the_exit_and_the_exit_is_named_after_it(
    #[fixture(unicode_home)] home: &Path,
) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![
        Launch::off(""),
        Launch::crashing("DEVVM-PROGRESS=first logon: x\r\nDEVVM-BASE-FAILED=boom\r\n"),
    ]));
    let (rig, backend) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    let message = error.message();
    assert!(
        message.starts_with("the Windows guest failed to build the base: boom"),
        "{message}"
    );
    assert!(message.contains("launch 2"), "{message}");
    assert!(message.contains("exit code: 1"), "{message}");
    assert!(message.contains(EXIT_OUTPUT), "{message}");
    rig.assert_names_no_build_file(&error);
    assert_eq!(backend.launches(), 2);
}

#[skuld::test]
fn a_done_marker_with_a_failing_exit_is_an_error_naming_both(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![Launch::crashing(&format!(
        "DEVVM-PROGRESS=first logon: x\r\n{LICENSED}\r\nDEVVM-BASE-DONE\r\n"
    ))]));
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    let message = error.message();
    assert!(message.contains("DEVVM-BASE-DONE"), "{message}");
    assert!(message.contains("launch 1"), "{message}");
    assert!(message.contains("exit code: 1"), "{message}");
    assert!(message.contains("incomplete"), "{message}");
    rig.assert_names_no_build_file(&error);
}

/// Puts a directory where the launch's serial log was, so that reading the log fails.
fn serial_log_becomes_a_directory(spec: &MachineSpec) {
    std::fs::remove_file(&spec.serial_log).unwrap();
    std::fs::create_dir(&spec.serial_log).unwrap();
}

#[skuld::test]
fn a_serial_log_that_cannot_be_read_while_the_guest_runs_still_gets_the_tail_and_the_winpe_logs(
    #[fixture(unicode_home)] home: &Path,
) {
    let winpe = Launch::running("firmware line\r\n").before(|spec| {
        guest_writes(&answer_disk(spec), "logs/winpe.log", b"last winpe line\r\n");
        serial_log_becomes_a_directory(spec);
    });
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![winpe]));
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    let message = error.message();
    assert!(message.contains("cannot read the serial log"), "{message}");
    assert!(message.contains("the end of the serial log"), "{message}");
    assert!(message.contains("last winpe line"), "{message}");
    rig.assert_names_no_build_file(&error);
}

#[skuld::test]
fn a_serial_log_that_cannot_be_read_after_the_guest_exited_gets_the_same(#[fixture(unicode_home)] home: &Path) {
    let winpe = Launch::off("firmware line\r\n").before(|spec| {
        guest_writes(&answer_disk(spec), "logs/winpe.log", b"last winpe line\r\n");
        serial_log_becomes_a_directory(spec);
    });
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![winpe]));
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    let message = error.message();
    assert!(message.contains("cannot read the serial log"), "{message}");
    assert!(message.contains("last winpe line"), "{message}");
    rig.assert_names_no_build_file(&error);
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
    let (rig, backend) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    let message = error.message();
    assert!(message.contains(german), "{message}");
    assert!(message.contains("the end of the serial log"), "{message}");
    rig.assert_names_no_build_file(&error);
    assert_eq!(backend.launches(), 2, "a failed first logon is not a Setup reboot");
}

#[skuld::test]
fn a_missing_license_marker_fails_the_build(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![Launch::off(
        "DEVVM-PROGRESS=first logon: x\r\nDEVVM-BASE-DONE\r\n",
    )]));
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().contains("DEVVM-LICENSE"), "{error}");
    assert!(error.message().contains("the end of the serial log"), "{error}");
    assert!(error.message().contains("DEVVM-BASE-DONE"), "{error}");
    rig.assert_names_no_build_file(&error);
}

#[skuld::test]
fn an_unreadable_license_marker_fails_the_build(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![finished("DEVVM-LICENSE=not json")]));
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert!(error.message().contains("DEVVM-LICENSE=not json"), "{error}");
    rig.assert_names_no_build_file(&error);
}

#[skuld::test]
fn the_license_marker_becomes_layer_meta_expires(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![finished(LICENSED)]));
    // The single image of this ISO is an evaluation edition.
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

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
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ExpiredBase, "{error}");
}

#[skuld::test]
fn a_base_with_less_than_ten_days_of_grace_is_expired_base_naming_the_date(#[fixture(unicode_home)] home: &Path) {
    let nine_days = LICENSED.replace("129600", &(9 * 24 * 60).to_string());
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![finished(&nine_days)]));
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ExpiredBase, "{error}");
    assert!(error.message().contains("within 10 days"), "{error}");
}

/// A license read whose grace is `minutes`.
fn licensed_for(minutes: u64) -> String {
    LICENSED.replace("129600", &minutes.to_string())
}

#[skuld::test]
fn a_base_with_exactly_ten_days_of_grace_is_expired_base(#[fixture(unicode_home)] home: &Path) {
    let ten_days = licensed_for(10 * 24 * 60);
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![finished(&ten_days)]));
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::ExpiredBase, "{error}");
}

#[skuld::test]
fn a_base_with_ten_days_and_a_minute_of_grace_is_accepted(#[fixture(unicode_home)] home: &Path) {
    let more = licensed_for(10 * 24 * 60 + 1);
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![finished(&more)]));
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

    let layer = rig.build(None).unwrap();

    let now: SystemTime = rig.env.clock.now();
    assert_eq!(
        layer.tip().meta().expires,
        Some(now + Duration::from_secs((10 * 24 * 60 + 1) * 60))
    );
}

#[skuld::test]
fn the_expiry_counts_from_when_the_guest_printed_the_license_not_from_the_end_of_the_build(
    #[fixture(unicode_home)] home: &Path,
) {
    // The build's clock is set forward in the last launch, after the license was printed in the one before.
    let clock = Arc::new(FakeClock::new(t0()));
    let later = clock.clone();
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![
        Launch::off(&format!("DEVVM-PROGRESS=first logon: x\r\n{LICENSED}\r\n")),
        Launch::off("DEVVM-PROGRESS=installing OpenSSH\r\nDEVVM-BASE-DONE\r\n")
            .before(move |_| later.set(t0() + days(5))),
    ]));
    let (mut rig, _) = rig(home, "single-image.iso", backend, Bounds::default());
    rig.ctx.clock = clock;

    let layer = rig.build(None).unwrap();

    assert_eq!(
        layer.tip().meta().expires,
        Some(t0() + Duration::from_secs(129_600 * 60))
    );
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
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

    let error = rig.build(None).unwrap_err();

    for (n, text) in [(1, "first"), (2, "second")] {
        assert!(text_of(&rig.serial_log(n)).contains(text), "serial-{n}.log");
    }
    assert!(error.message().contains("boom"), "{error}");
    assert!(
        !error.message().contains("second"),
        "only the failed launch's log: {error}"
    );
}

#[skuld::test]
fn progress_markers_are_echoed_as_progress_lines(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![
        Launch::off(""),
        Launch::off("DEVVM-PROGRESS=specialize\r\n"),
        finished(LICENSED),
    ]));
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

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
    let (rig, _) = rig(home, "single-image.iso", backend, Bounds::default());

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
    let winpe = Launch::running("firmware line\r\n").before(|spec| {
        guest_writes(
            &answer_disk(spec),
            "logs/winpe.log",
            b"DEVVM-WINPE-FAILED=diskpart\r\nlast winpe line\r\n",
        );
    });
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![winpe]));
    let (rig, backend) = rig(
        home,
        "single-image.iso",
        backend,
        bounds(Duration::from_millis(300), HOUR),
    );

    let error = rig.build(None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    let message = error.message();
    assert!(message.contains("Windows Setup"), "{message}");
    assert!(message.contains("within 300ms"), "{message}");
    assert!(message.contains("firmware line"), "{message}");
    rig.assert_names_no_build_file(&error);
    assert!(message.contains("logs/winpe.log"), "{message}");
    assert!(message.contains("last winpe line"), "{message}");
    assert_eq!(backend.launches(), 1);
}

#[skuld::test]
fn a_stall_after_setup_names_the_first_logon_script_and_not_winpes_logs(#[fixture(unicode_home)] home: &Path) {
    let logon = Launch::running("DEVVM-PROGRESS=first logon: x\r\n").before(|spec| {
        guest_writes(&answer_disk(spec), "logs/winpe.log", b"old winpe line");
    });
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![logon]));
    let (rig, _) = rig(
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
fn a_guest_that_finished_as_its_bound_passed_is_finished_not_stalled(#[fixture(unicode_home)] home: &Path) {
    // The VM process has exited when the stall is found: the first look says it still runs, and the bound is already
    // past (no first-logon marker has moved the build out of the phase with the zero bound).
    let backend = FakeBackend::new(&TestHost)
        .with_guest(guest(vec![Launch::off(&format!("{LICENSED}\r\nDEVVM-BASE-DONE\r\n"))]))
        .with_late_exit_status();
    let (rig, _) = rig(home, "single-image.iso", backend, bounds(Duration::ZERO, HOUR));

    rig.build(None).unwrap();
}

#[skuld::test]
fn a_stalled_guest_is_stopped_and_its_files_are_removed(#[fixture(unicode_home)] home: &Path) {
    let backend = FakeBackend::new(&TestHost).with_guest(guest(vec![Launch::running("")]));
    let (rig, backend) = rig(
        home,
        "single-image.iso",
        backend,
        bounds(Duration::from_millis(200), HOUR),
    );

    rig.build(None).unwrap_err();

    let launched = backend.launched();
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
    let (rig, _) = rig(
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
