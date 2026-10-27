use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cosca::identity::{Platform, ProcessIdRecord};

use crate::backend::{Backend, Endpoint, Machine, MachineRecord, SshEndpoint, Storage};
use crate::guest::{BaseBuilder, GuestOs, Guests};
use crate::test_support::fixtures::anna_home;

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
        ],
    }
}

const GOLDEN: &str = r#"{"process":{"v":1,"platform":"windows","pid":4242,"token":"133000000000000000"},"qmp":"/run/user/1000/devvm-ab.sock","ssh":[{"endpoint":"normal","addr":"127.0.0.1:2222"},{"endpoint":"elevated","addr":"127.0.0.1:2223"}],"serial_log":"/var/devvm/serial.log","leftovers":["/run/user/1000/devvm-ab.sock","/run/user/1000/devvm-ab.sock.lock"]}"#;

#[skuld::test]
fn machine_record_json_is_stable() {
    let record = sample();
    assert_eq!(serde_json::to_string(&record).unwrap(), GOLDEN);
    assert_eq!(serde_json::from_str::<MachineRecord>(GOLDEN).unwrap(), record);
}

#[skuld::test]
fn a_machine_record_without_leftovers_parses_with_none() {
    let without = GOLDEN.replace(
        r#","leftovers":["/run/user/1000/devvm-ab.sock","/run/user/1000/devvm-ab.sock.lock"]"#,
        "",
    );
    assert_ne!(without, GOLDEN);
    let parsed = serde_json::from_str::<MachineRecord>(&without).unwrap();
    assert!(parsed.leftovers.is_empty());
    assert_eq!(parsed.serial_log, sample().serial_log);
}

#[skuld::test]
fn remove_leftovers_deletes_files_ignores_absent_ones_and_warns_on_the_rest(#[fixture(anna_home)] home: &Path) {
    let (gone, kept_by_warning, absent) = (home.join("a.sock"), home.join("stuck"), home.join("never"));
    std::fs::write(&gone, b"x").unwrap();
    // A directory cannot be removed as a file.
    std::fs::create_dir(&kept_by_warning).unwrap();
    let record = MachineRecord {
        leftovers: vec![kept_by_warning.clone(), gone.clone(), absent],
        ..sample()
    };

    let mut warnings = Vec::new();
    record.remove_leftovers(&mut |text| warnings.push(text));
    assert!(!gone.exists(), "a failure on one file does not stop the others");
    assert!(kept_by_warning.exists());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("stuck"), "{warnings:?}");
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
