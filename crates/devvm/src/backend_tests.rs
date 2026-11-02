use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cosca::identity::{Platform, ProcessIdRecord};

use crate::backend::{Backend, Endpoint, FileClaim, Machine, MachineRecord, SshEndpoint, Storage, remove_files};
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
    let warnings = removed(&paths, Some(&other));
    assert!(
        warnings.iter().any(|w| w.contains("another launch's file")),
        "{warnings:?}"
    );
    assert!(paths[0].exists() && paths[1].exists() && paths[2].exists());
}

#[skuld::test]
fn remove_files_deletes_nothing_when_the_claim_file_is_gone_and_others_remain(#[fixture(unicode_home)] home: &Path) {
    let (paths, claim) = launch_files(home, "a");
    std::fs::remove_file(&claim.path).unwrap();
    let warnings = removed(&paths, Some(&claim));
    assert!(warnings.iter().any(|w| w.contains("is gone")), "{warnings:?}");
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

/// A machine that is alive holds the claim file, so its files are not a dead machine's.
#[skuld::test]
fn remove_files_deletes_nothing_while_the_claim_is_held(#[fixture(unicode_home)] home: &Path) {
    let (paths, claim) = launch_files(home, "a");
    let alive = held(&claim.path);
    let warnings = removed(&paths, Some(&claim));
    assert!(warnings.iter().any(|w| w.contains("is in use")), "{warnings:?}");
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
