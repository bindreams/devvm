use std::path::Path;
use std::sync::Arc;

use crate::backend::fake::{FakeBackend, FakeStorage};
use crate::backend::{DeviceProfile, DiskBus, NetModel, Video};
use crate::exit::NullProgress;
use crate::guest::linux::{LinuxBuilder, LinuxGuest, QemuGuests};
use crate::guest::windows::builder::{Bounds, WindowsBuilder};
use crate::guest::{GuestOs, Guests, Readiness};
use crate::recipe::{Setting, Shell, UacMode};
use crate::store::fetch::{Fetcher, HttpTransport};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::host::TestHost;
use crate::test_support::sshd::Rig;
use crate::{Command, Dirs, ErrorKind, GuestKind, RunAs};

#[skuld::test]
fn shell_argv_cases() {
    let argv = |shell, script| LinuxGuest.shell_argv(shell, script).unwrap();
    assert_eq!(argv(None, "echo hi"), ["bash", "-c", "echo hi"]);
    assert_eq!(argv(Some(Shell::Bash), "echo hi"), ["bash", "-c", "echo hi"]);
    assert_eq!(argv(Some(Shell::Sh), "echo hi"), ["sh", "-c", "echo hi"]);
    assert_eq!(
        argv(Some(Shell::Pwsh), "Write-Output hi"),
        ["pwsh", "-NoProfile", "-Command", "Write-Output hi"]
    );
    let error = LinuxGuest.shell_argv(Some(Shell::Cmd), "dir").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("cmd"), "{}", error.message());
}

#[skuld::test]
fn semantics_version_is_one() {
    assert_eq!(LinuxGuest.semantics_version(), 1);
}

#[skuld::test]
fn virtio_devices_and_a_guest_that_reboots_inside_its_process() {
    assert_eq!(
        LinuxGuest.device_profile(),
        DeviceProfile {
            disk: DiskBus::Virtio,
            net: NetModel::Virtio,
            video: Video::None,
            no_reboot: false,
        }
    );
}

// The `Guests` and `GuestOs` implementations ==========================================================================

fn guests(home: &Path) -> QemuGuests {
    let fetcher = Arc::new(Fetcher::new(
        Dirs::under(home).downloads(),
        Arc::new(HttpTransport::new().unwrap()),
        Arc::new(NullProgress),
    ));
    QemuGuests::new(
        Arc::new(LinuxBuilder::new(fetcher.clone(), Arc::new(FakeStorage::default()))),
        Arc::new(WindowsBuilder::new(
            fetcher,
            Arc::new(FakeBackend::new(&TestHost)),
            &TestHost,
            Bounds::default(),
        )),
    )
}

fn os(home: &Path) -> Arc<dyn GuestOs> {
    guests(home).os(GuestKind::Linux).unwrap()
}

#[skuld::test]
fn the_qemu_guests_serve_linux_and_windows_and_name_the_missing_ones(#[fixture(unicode_home)] home: &Path) {
    let guests = guests(home);
    guests.os(GuestKind::Linux).unwrap();
    guests.os(GuestKind::Windows).unwrap();
    guests.builder(GuestKind::Linux).unwrap();
    guests.builder(GuestKind::Windows).unwrap();
    let errors = [
        guests.os(GuestKind::Macos).err().unwrap(),
        guests.builder(GuestKind::Macos).err().unwrap(),
    ];
    for error in errors {
        let kind = GuestKind::Macos;
        assert_eq!(error.kind(), ErrorKind::NotSupported);
        assert!(error.message().contains(kind.as_str()), "{}", error.message());
    }
}

#[skuld::test]
fn a_linux_guest_refuses_every_setting_naming_itself_and_the_setting() {
    let error = LinuxGuest
        .setting_command(&Setting::Uac(UacMode::AutoApprove))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(
        error.message().contains("linux") && error.message().contains("set.uac"),
        "{error}"
    );
}

#[skuld::test]
fn the_trait_object_wraps_and_describes_like_the_linux_guest(#[fixture(unicode_home)] home: &Path) {
    let os = os(home);
    let cmd = Command::new(["echo", "hi"]);
    let wrapped = os.wrap(&cmd).unwrap();
    assert_eq!(wrapped, r#"bash -lc 'exec -- '\''echo'\'' '\''hi'\'''"#);
    assert_eq!(os.boot_id_command().argv, ["cat", "/proc/sys/kernel/random/boot_id"]);
    assert_eq!(os.flush_command().argv[..2], ["sh", "-c"]);
    assert_eq!(os.semantics_version(), 1);
    assert_eq!(os.device_profile().disk, DiskBus::Virtio);
    assert_eq!(os.shell_argv(Some(Shell::Sh), "true").unwrap(), ["sh", "-c", "true"]);
}

/// Ubuntu's cloud image mounts `/boot/efi` readable by root only, so a flush run as the user fails on every build.
#[skuld::test]
fn the_flush_runs_as_root_so_a_mount_point_the_user_cannot_open_still_flushes() {
    assert_eq!(LinuxGuest.flush_command().run_as, RunAs::Elevated);
}

#[skuld::test]
fn sftp_path_is_total_and_expands_tilde_forms(#[fixture(unicode_home)] home: &Path) {
    let os = os(home);
    for (typed, home_dir, sent) in [
        ("~", "/home/user", "/home/user"),
        ("~/x", "/home/user", "/home/user/x"),
        ("~/x/y", "/home/user/", "/home/user/x/y"),
        ("~", "/", "/"),
        ("~/x", "/", "/x"),
        ("~user/x", "/home/user", "~user/x"),
        ("~root", "/home/user", "~root"),
        ("/etc/hosts", "/home/user", "/etc/hosts"),
        ("rel/x", "/home/user", "rel/x"),
        ("", "/home/user", ""),
        ("~/\u{416}\u{1F600}", "/home/\u{e9}", "/home/\u{e9}/\u{416}\u{1F600}"),
    ] {
        assert_eq!(os.sftp_path(typed, home_dir), sent, "{typed:?} with home {home_dir:?}");
    }
}

#[skuld::test]
fn user_path_undoes_sftp_path_and_names_what_a_tilde_stood_for(#[fixture(unicode_home)] home: &Path) {
    let os = os(home);
    for typed in [
        "/",
        "/a",
        "/a/b.txt",
        "rel/x",
        "",
        "~user/x",
        "/home/user",
        "/home/user/x",
    ] {
        let sent = os.sftp_path(typed, "/home/user");
        assert_eq!(os.user_path(&sent), typed, "{typed:?} was sent as {sent:?}");
    }
    // The tilde is gone from what was sent, so the path it stood for is the closest the user's own words can be.
    for typed in ["~", "~/x"] {
        let sent = os.sftp_path(typed, "/home/user");
        assert_eq!(os.user_path(&sent), sent);
    }
}

#[skuld::test]
fn split_root_knows_one_root_and_no_drives(#[fixture(unicode_home)] home: &Path) {
    let os = os(home);
    for (path, root, rest) in [
        ("/", "/", ""),
        ("/a/b", "/", "a/b"),
        ("//a", "/", "a"),
        ("///a/b", "/", "a/b"),
        ("a/b", "", "a/b"),
        ("", "", ""),
        ("./a", "", "./a"),
        ("C:/x", "", "C:/x"),
        (r"\server\share", "", r"\server\share"),
    ] {
        let (found, tail) = os.split_root(path).unwrap();
        assert_eq!((found.as_str(), tail), (root, rest), "{path:?}");
    }
}

#[skuld::test]
fn file_names_follow_the_unix_rules(#[fixture(unicode_home)] home: &Path) {
    let os = os(home);
    for name in ["a?b", "a:b", "CON", "trailing.", "a b", "\u{416}", &"x".repeat(255)] {
        assert_eq!(os.file_name_problem(name), None, "{name:?}");
    }
    for name in ["", "a/b", "a\0b", &"x".repeat(256)] {
        assert!(os.file_name_problem(name).is_some(), "{name:?}");
    }
}

fn ready_with(home: &Path, code: u32, reply: &str) -> crate::Result<Readiness> {
    let reply = reply.to_owned();
    let rig = Rig::with(home, |options| options.cloud_init = Some((code, reply)));
    let session = rig.connect();
    os(home).wait_ready(&session)
}

#[skuld::test]
fn wait_ready_runs_cloud_inits_status_and_reads_its_reply(#[fixture(unicode_home)] home: &Path) {
    assert_eq!(
        ready_with(
            home,
            0,
            r#"{"status": "done", "extended_status": "done", "errors": []}"#
        )
        .unwrap(),
        Readiness::Ready
    );
    let degraded = ready_with(
        home,
        2,
        r#"{"status": "done", "extended_status": "degraded done", "errors": [], "recoverable_errors": {"WARNING": ["x"]}}"#,
    )
    .unwrap();
    assert!(
        matches!(&degraded, Readiness::Degraded(text) if text.contains("degraded done")),
        "{degraded:?}"
    );
    let error = ready_with(home, 1, r#"{"status": "error", "errors": ["boom"]}"#).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains("boom"), "{}", error.message());
}

#[skuld::test]
fn wait_ready_on_a_guest_without_cloud_init_is_an_error_carrying_the_output(#[fixture(unicode_home)] home: &Path) {
    let rig = Rig::new(home);
    let error = os(home).wait_ready(&rig.connect()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains("127"), "{}", error.message());
}
