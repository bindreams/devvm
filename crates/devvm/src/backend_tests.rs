use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cosca::identity::{Platform, ProcessIdRecord};

use crate::backend::{
    Backend, Endpoint, FileClaim, Machine, MachineRecord, SshEndpoint, Storage, directory_of, make_overlay,
    remove_files,
};
use crate::guest::{BaseBuilder, GuestOs, Guests};
use crate::host::{self, FileId, HoldOutcome};
use crate::test_support::fixtures::unicode_home;

fn sample() -> MachineRecord {
    MachineRecord {
        process: ProcessIdRecord {
            version: 1,
            platform: Platform::Windows,
            pid: 4242,
            token: 133_000_000_000_000_000,
            boot_id: None,
            pid_ns: None,
        },
        qmp: "/run/user/1000/devvm-ab.sock".to_owned(),
        ssh: vec![
            SshEndpoint {
                endpoint: Endpoint::Normal,
                addr: "127.0.0.1:2222".parse::<SocketAddr>().unwrap(),
            },
            SshEndpoint {
                endpoint: Endpoint::Elevated,
                addr: "127.0.0.1:2223".parse::<SocketAddr>().unwrap(),
            },
        ],
        serial_log: PathBuf::from("/var/devvm/serial.log"),
        leftovers: vec![
            PathBuf::from("/run/user/1000/devvm-ab.sock"),
            PathBuf::from("/run/user/1000/devvm-ab.sock.lock"),
            PathBuf::from("/run/user/1000/devvm-ab.sock.hold"),
        ],
        claim: Some(FileClaim {
            path: PathBuf::from("/run/user/1000/devvm-ab.sock.hold"),
            id: FileId {
                volume: 0x1a,
                file: 0xff,
            },
        }),
        no_reboot: false,
    }
}

const GOLDEN: &str = r#"{"process":{"v":1,"platform":"windows","pid":4242,"token":"133000000000000000"},"qmp":"/run/user/1000/devvm-ab.sock","ssh":[{"endpoint":"normal","addr":"127.0.0.1:2222"},{"endpoint":"elevated","addr":"127.0.0.1:2223"}],"serial_log":"/var/devvm/serial.log","leftovers":["/run/user/1000/devvm-ab.sock","/run/user/1000/devvm-ab.sock.lock","/run/user/1000/devvm-ab.sock.hold"],"claim":{"path":"/run/user/1000/devvm-ab.sock.hold","id":"1a:ff"},"no_reboot":false}"#;

#[skuld::test]
fn machine_record_json_is_stable() {
    let record = sample();
    assert_eq!(serde_json::to_string(&record).unwrap(), GOLDEN);
    assert_eq!(serde_json::from_str::<MachineRecord>(GOLDEN).unwrap(), record);
}

#[skuld::test]
fn a_machine_record_without_no_reboot_parses_as_a_machine_that_reboots() {
    let without = GOLDEN.replace(r#","no_reboot":false"#, "");
    assert_ne!(without, GOLDEN);
    let parsed = serde_json::from_str::<MachineRecord>(&without).unwrap();
    assert!(!parsed.no_reboot);
    assert_eq!(parsed, sample());
}

#[skuld::test]
fn a_machine_record_keeps_no_reboot() {
    let record = MachineRecord {
        no_reboot: true,
        ..sample()
    };
    let json = serde_json::to_string(&record).unwrap();
    assert!(json.contains(r#""no_reboot":true"#), "{json}");
    assert_eq!(serde_json::from_str::<MachineRecord>(&json).unwrap(), record);
}

#[skuld::test]
fn a_machine_record_without_leftovers_parses_with_none() {
    let without = GOLDEN.replace(
        r#","leftovers":["/run/user/1000/devvm-ab.sock","/run/user/1000/devvm-ab.sock.lock","/run/user/1000/devvm-ab.sock.hold"],"claim":{"path":"/run/user/1000/devvm-ab.sock.hold","id":"1a:ff"}"#,
        "",
    );
    assert_ne!(without, GOLDEN);
    let parsed = serde_json::from_str::<MachineRecord>(&without).unwrap();
    assert!(parsed.leftovers.is_empty());
    assert_eq!(parsed.claim, None);
    assert_eq!(parsed.serial_log, sample().serial_log);
}

/// The files of a launch under `dir`: a socket, a session lock and the hold that is the claim, which nobody holds now.
fn launch_files(dir: &Path, name: &str) -> (Vec<PathBuf>, FileClaim) {
    let (socket, lock, hold) = (
        dir.join(format!("{name}.sock")),
        dir.join(format!("{name}.sock.lock")),
        dir.join(format!("{name}.sock.hold")),
    );
    std::fs::write(&socket, b"").unwrap();
    std::fs::write(&lock, b"").unwrap();
    let made = host::current().hold_new(&hold).unwrap();
    let claim = FileClaim {
        id: made.id().unwrap(),
        path: hold.clone(),
    };
    drop(made);
    (vec![socket, lock, hold], claim)
}

fn removed(paths: &[PathBuf], claim: Option<&FileClaim>) -> Vec<String> {
    let mut warnings = Vec::new();
    remove_files(host::current(), paths, claim, &mut |text| warnings.push(text));
    warnings
}

fn held(path: &Path) -> Box<dyn crate::host::Hold> {
    match host::current().hold_existing(path).unwrap() {
        HoldOutcome::Taken(hold) => hold,
        other => panic!("expected to take the hold on {}, got {other:?}", path.display()),
    }
}

#[skuld::test]
fn remove_files_deletes_the_files_and_then_the_claim(#[fixture(unicode_home)] home: &Path) {
    let (paths, claim) = launch_files(home, "a");
    assert_eq!(removed(&paths, Some(&claim)), Vec::<String>::new());
    for path in &paths {
        assert!(!path.exists(), "{} was left", path.display());
    }
}

#[skuld::test]
fn remove_files_is_quiet_when_everything_is_gone_already(#[fixture(unicode_home)] home: &Path) {
    let (paths, claim) = launch_files(home, "a");
    removed(&paths, Some(&claim));
    assert_eq!(removed(&paths, Some(&claim)), Vec::<String>::new());
    assert_eq!(removed(&paths, None), Vec::<String>::new());
}

#[skuld::test]
fn remove_files_deletes_nothing_when_the_claim_file_is_another_file(#[fixture(unicode_home)] home: &Path) {
    let (paths, claim) = launch_files(home, "a");
    // What a later launch's file at the same path looks like to a record of the first: another id. (A file system may
    // give a deleted file's number to the next one, so the test names the id instead of making a second file.)
    let other = FileClaim {
        id: FileId {
            file: claim.id.file ^ 1,
            ..claim.id
        },
        ..claim
    };
    // The files belong to that launch; a sweep reclaims them once it is dead.
    assert_eq!(removed(&paths, Some(&other)), Vec::<String>::new());
    assert!(paths[0].exists() && paths[1].exists() && paths[2].exists());
}

#[skuld::test]
fn remove_files_deletes_nothing_when_the_claim_file_is_gone_and_others_remain(#[fixture(unicode_home)] home: &Path) {
    let (paths, claim) = launch_files(home, "a");
    std::fs::remove_file(&claim.path).unwrap();
    // Whoever deleted the claim deleted the other files first; what is there now is a later launch's.
    assert_eq!(removed(&paths, Some(&claim)), Vec::<String>::new());
    assert!(paths[0].exists() && paths[1].exists());
}

#[skuld::test]
fn remove_files_deletes_nothing_without_a_claim_and_names_the_files(#[fixture(unicode_home)] home: &Path) {
    let (paths, _claim) = launch_files(home, "a");
    let warnings = removed(&paths, None);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("a.sock") && warnings[0].contains("no launch claims"),
        "{warnings:?}"
    );
    assert!(paths.iter().all(|path| path.exists()));
}

/// A file that cannot be looked at is not known to be gone: it is named, not skipped.
#[skuld::test]
fn remove_files_names_a_file_that_cannot_be_looked_at_instead_of_taking_it_for_gone(
    #[fixture(unicode_home)] home: &Path,
) {
    let unlookable = home.join("a\0b");

    let warnings = removed(&[unlookable], None);

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("no launch claims"), "{warnings:?}");
}

/// A machine that is alive holds the claim file, so its files are not a dead machine's.
#[skuld::test]
fn remove_files_deletes_nothing_while_the_claim_is_held(#[fixture(unicode_home)] home: &Path) {
    let (paths, claim) = launch_files(home, "a");
    let alive = held(&claim.path);
    // Another deleter or an inheritor owns the cleanup.
    assert_eq!(removed(&paths, Some(&claim)), Vec::<String>::new());
    assert!(paths.iter().all(|path| path.exists()));
    drop(alive);
    removed(&paths, Some(&claim));
    assert!(paths.iter().all(|path| !path.exists()));
}

#[skuld::test]
fn remove_files_warns_on_a_failure_keeps_the_claim_and_still_deletes_the_others(#[fixture(unicode_home)] home: &Path) {
    let (mut paths, claim) = launch_files(home, "a");
    // A directory cannot be removed as a file.
    let stuck = home.join("stuck");
    std::fs::create_dir(&stuck).unwrap();
    paths.insert(0, stuck);
    let warnings = removed(&paths, Some(&claim));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("stuck"), "{warnings:?}");
    assert!(
        !paths[1].exists() && !paths[2].exists(),
        "a failure on one file does not stop the others"
    );
    assert!(
        claim.path.exists(),
        "the claim stays while a file does, so the names stay reserved"
    );
}

/// The case the claim exists for: the first launch's files are gone, a later launch drew its name, and the first
/// launch's record is acted on late.
#[skuld::test]
fn a_stale_record_does_not_delete_the_files_of_a_later_launch_at_the_same_name(#[fixture(unicode_home)] home: &Path) {
    let (first, first_claim) = launch_files(home, "a");
    removed(&first, Some(&first_claim));
    let (second, _second_claim) = launch_files(home, "a");
    // The later launch is alive: it holds its claim file.
    let alive = held(&second[2]);

    removed(&first, Some(&first_claim));

    assert!(second.iter().all(|path| path.exists()));
    drop(alive);
}

#[skuld::test]
fn machine_record_with_unknown_fields_still_parses() {
    let with_extras = GOLDEN
        .replacen('{', r#"{"future":1,"#, 1)
        .replace(r#""endpoint":"normal""#, r#""endpoint":"normal","note":"x""#);
    assert_eq!(serde_json::from_str::<MachineRecord>(&with_extras).unwrap(), sample());
}

#[skuld::test]
fn traits_are_object_safe_and_thread_safe() {
    fn thread_safe<T: ?Sized + Send + Sync>() {}
    fn sendable<T: ?Sized + Send>() {}
    thread_safe::<dyn Backend>();
    thread_safe::<dyn Storage>();
    thread_safe::<dyn GuestOs>();
    thread_safe::<dyn BaseBuilder>();
    thread_safe::<dyn Guests>();
    sendable::<dyn Machine>();
    let _: Option<Arc<dyn Backend>> = None;
    let _: Option<Box<dyn Machine>> = None;
}

/// A file that was held and has been unlinked, and a later launch has the name: the earlier holder must not delete the
/// later launch's files. (A held file cannot be unlinked on Windows.)
#[cfg(unix)]
#[skuld::test]
fn remove_held_deletes_nothing_when_the_name_now_belongs_to_another_file(#[fixture(unicode_home)] home: &Path) {
    let (paths, claim) = launch_files(home, "a");
    let stale = held(&claim.path);
    std::fs::remove_file(&claim.path).unwrap();
    let (later, _later_claim) = launch_files(home, "a");

    let mut warnings = Vec::new();
    crate::backend::remove_held(host::current(), stale, &claim.path, &paths, &mut |text| {
        warnings.push(text)
    });

    assert_eq!(warnings, Vec::<String>::new());
    assert!(later.iter().all(|path| path.exists()));
}

// make_overlay --------------------------------------------------------------------------------------------------------

/// The names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_unstable();
    names
}

fn write_to(staging: &Path, bytes: &[u8]) -> crate::Result<()> {
    std::fs::write(staging, bytes)
        .map_err(|e| crate::Error::io(crate::ErrorKind::Io, "cannot write the staging file", e))
}

#[skuld::test]
fn an_overlay_is_written_beside_its_name_and_published_whole(#[fixture(unicode_home)] home: &Path) {
    let into = home.join("overlay.qcow2");

    make_overlay(&into, |staging, directory| {
        assert_eq!(directory, home, "the fill is handed the directory of the overlay");
        assert_eq!(
            staging.parent(),
            Some(home),
            "the staging file must be on the volume of the overlay"
        );
        assert_ne!(staging, into);
        assert!(!into.exists(), "the overlay is visible before it is whole");
        write_to(staging, b"overlay")
    })
    .unwrap();

    assert_eq!(std::fs::read(&into).unwrap(), b"overlay");
    assert_eq!(names_in(home), ["overlay.qcow2"], "the staging file is gone");
}

#[skuld::test]
fn an_overlay_that_fails_after_it_was_started_leaves_nothing(#[fixture(unicode_home)] home: &Path) {
    let into = home.join("overlay.qcow2");

    let error = make_overlay(&into, |staging, _| {
        write_to(staging, b"half an overlay")?;
        Err(crate::Error::new(crate::ErrorKind::Io, "the patch did not go in"))
    })
    .unwrap_err();

    assert_eq!(error.kind(), crate::ErrorKind::Io, "{error}");
    assert_eq!(error.message(), "the patch did not go in");
    assert_eq!(names_in(home), Vec::<String>::new());
}

#[skuld::test]
fn two_calls_for_one_overlay_the_first_to_publish_keeps_it_and_the_other_is_refused(
    #[fixture(unicode_home)] home: &Path,
) {
    let into = home.join("overlay.qcow2");

    // The second call checks and starts while the first is still writing, and publishes first.
    let error = make_overlay(&into, |first, _| {
        write_to(first, b"first, unfinished")?;
        make_overlay(&into, |second, _| write_to(second, b"second")).unwrap();
        Ok(())
    })
    .unwrap_err();

    assert_eq!(error.kind(), crate::ErrorKind::AlreadyExists, "{error}");
    assert!(error.message().contains("overlay.qcow2"), "{error}");
    assert_eq!(
        std::fs::read(&into).unwrap(),
        b"second",
        "the first call overwrote the second's overlay"
    );
    assert_eq!(names_in(home), ["overlay.qcow2"], "a staging file was left");
}

#[skuld::test]
fn a_call_that_fails_never_deletes_the_overlay_another_call_published(#[fixture(unicode_home)] home: &Path) {
    let into = home.join("overlay.qcow2");

    let error = make_overlay(&into, |first, _| {
        write_to(first, b"first, unfinished")?;
        make_overlay(&into, |second, _| write_to(second, b"second")).unwrap();
        Err(crate::Error::new(crate::ErrorKind::Io, "the patch did not go in"))
    })
    .unwrap_err();

    assert_eq!(error.message(), "the patch did not go in");
    assert_eq!(
        std::fs::read(&into).unwrap(),
        b"second",
        "the failed call deleted the other call's overlay"
    );
    assert_eq!(names_in(home), ["overlay.qcow2"]);
}

#[skuld::test]
fn a_call_touches_nothing_in_the_directory_but_its_own_files(#[fixture(unicode_home)] home: &Path) {
    let into = home.join("overlay.qcow2");
    // A leftover of a killed call is the caller's to clean; a file that merely starts like a staging file is not ours.
    std::fs::write(home.join("overlay-killed"), b"half an overlay").unwrap();
    std::fs::write(home.join("patch-killed"), b"half a patch").unwrap();

    make_overlay(&into, |staging, _| write_to(staging, b"overlay")).unwrap();

    assert_eq!(
        names_in(home),
        ["overlay-killed", "overlay.qcow2", "patch-killed"],
        "the call removed a file it did not make"
    );
}

#[skuld::test]
fn an_overlay_with_no_directory_is_refused_before_anything_is_made() {
    let error = make_overlay(Path::new("overlay.qcow2"), |_, _| panic!("nothing is made")).unwrap_err();

    assert_eq!(error.kind(), crate::ErrorKind::Internal, "{error}");
    assert!(error.message().contains("the overlay"), "{error}");
}

#[skuld::test]
fn a_path_without_a_directory_is_refused_naming_what_it_is() {
    // `Path::parent` gives an empty path for a bare file name, which would resolve against the working directory.
    for path in ["", "disk.img", ".", "/"] {
        let error = directory_of(Path::new(path), "the machine's disk").unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::Internal, "{path:?}: {error}");
        assert!(error.message().contains("the machine's disk"), "{path:?}: {error}");
    }
}

#[skuld::test]
fn a_path_in_a_directory_gives_the_directory() {
    let dir = std::env::temp_dir();
    assert_eq!(directory_of(&dir.join("disk.img"), "the disk").unwrap(), dir);
}
