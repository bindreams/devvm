use std::net::SocketAddr;
use std::sync::Arc;

use cosca::identity::{Platform, ProcessIdRecord};

use crate::backend::{Backend, Endpoint, Machine, MachineRecord, SshEndpoint, Storage};
use crate::guest::{BaseBuilder, GuestOs, Guests};

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
    }
}

const GOLDEN: &str = r#"{"process":{"v":1,"platform":"windows","pid":4242,"token":"133000000000000000"},"qmp":"/run/user/1000/devvm-ab.sock","ssh":[{"endpoint":"normal","addr":"127.0.0.1:2222"},{"endpoint":"elevated","addr":"127.0.0.1:2223"}]}"#;

#[skuld::test]
fn machine_record_json_is_stable() {
    let record = sample();
    assert_eq!(serde_json::to_string(&record).unwrap(), GOLDEN);
    assert_eq!(serde_json::from_str::<MachineRecord>(GOLDEN).unwrap(), record);
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
