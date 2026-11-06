use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use cosca::identity::{Platform, ProcessIdRecord};
use secrecy::ExposeSecret;

use crate::ErrorKind;
use crate::access::keys::InstanceKeys;
use crate::backend::{Endpoint, MachineRecord, SshEndpoint};
use crate::engine::engine_tests::{engine_fixture, instance_in, memory, record_machine};
use crate::engine::keys::{SSH_USER, ensure_keys, read_target};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::host::TestHost;

const FILES: [&str; 4] = ["client", "client.pub", "host", "host.pub"];

fn bytes(dir: &Path) -> Vec<Vec<u8>> {
    FILES
        .iter()
        .map(|name| std::fs::read(dir.join(name)).unwrap())
        .collect()
}

/// A record of a machine whose SSH endpoint is `addr`; nothing runs behind it.
fn machine_at(addr: SocketAddr, serial_log: &Path) -> MachineRecord {
    MachineRecord {
        process: ProcessIdRecord {
            version: 1,
            platform: Platform::Windows,
            pid: 4242,
            token: 1,
            boot_id: None,
            pid_ns: None,
        },
        qmp: String::new(),
        ssh: vec![SshEndpoint {
            endpoint: Endpoint::Normal,
            addr,
        }],
        serial_log: serial_log.to_path_buf(),
        leftovers: Vec::new(),
        claim: None,
        no_reboot: false,
    }
}

#[skuld::test]
fn keys_are_generated_at_the_first_call_and_read_at_every_later_one(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let dir = instance_in(&fx, "foo", home);
    let op = dir.op_exclusive().unwrap();
    let keys_dir = dir.path().join("keys");

    let first = ensure_keys(&dir, &op, &TestHost, &mut |w| panic!("no warning expected: {w}")).unwrap();
    let written = bytes(&keys_dir);
    let second = ensure_keys(&dir, &op, &TestHost, &mut |w| panic!("no warning expected: {w}")).unwrap();

    assert_eq!(bytes(&keys_dir), written, "a later call leaves the files as they were");
    assert_eq!(first.host_public, second.host_public, "the guest keeps its host key");
    assert_eq!(first.client_public, second.client_public);
    assert_eq!(first.host_private.expose_secret(), second.host_private.expose_secret());
    assert_eq!(
        InstanceKeys::read(&keys_dir).unwrap().client_public,
        first.client_public,
        "what is returned is what is on disk"
    );
}

#[skuld::test]
fn a_half_deleted_key_set_is_an_error_naming_the_directory_and_is_not_regenerated(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = engine_fixture(home, memory());
    let dir = instance_in(&fx, "foo", home);
    let op = dir.op_exclusive().unwrap();
    let keys_dir = dir.path().join("keys");
    ensure_keys(&dir, &op, &TestHost, &mut |_| {}).unwrap();
    std::fs::remove_file(keys_dir.join("host.pub")).unwrap();
    let before: Vec<_> = ["client", "client.pub", "host"]
        .iter()
        .map(|name| std::fs::read(keys_dir.join(name)).unwrap())
        .collect();

    let error = ensure_keys(&dir, &op, &TestHost, &mut |_| {})
        .expect_err("a set with a file missing is not a set to start from");

    assert_eq!(error.kind(), ErrorKind::AlreadyExists, "{error}");
    assert!(error.message().contains(&keys_dir.display().to_string()), "{error}");
    assert!(!keys_dir.join("host.pub").exists(), "no key was made to fill the gap");
    let after: Vec<_> = ["client", "client.pub", "host"]
        .iter()
        .map(|name| std::fs::read(keys_dir.join(name)).unwrap())
        .collect();
    assert_eq!(after, before, "the other three are unchanged");
}

#[skuld::test]
fn an_empty_keys_directory_is_filled_not_refused(#[fixture(unicode_home)] home: &Path) {
    // What a writer killed between its two steps leaves.
    let fx = engine_fixture(home, memory());
    let dir = instance_in(&fx, "foo", home);
    let op = dir.op_exclusive().unwrap();
    let keys_dir = dir.path().join("keys");
    std::fs::create_dir(&keys_dir).unwrap();

    let keys = ensure_keys(&dir, &op, &TestHost, &mut |_| {}).unwrap();

    assert_eq!(InstanceKeys::read(&keys_dir).unwrap().host_public, keys.host_public);
}

#[skuld::test]
fn what_the_key_write_could_not_clean_up_goes_to_the_warning_callback(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let dir = instance_in(&fx, "foo", home);
    let op = dir.op_exclusive().unwrap();
    // A file where a temporary directory should be: it cannot be removed as one.
    std::fs::write(dir.path().join(".keys-stuck"), "not a directory").unwrap();
    let mut warnings = Vec::new();

    ensure_keys(&dir, &op, &TestHost, &mut |w| warnings.push(w)).unwrap();

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains(".keys-stuck"), "{warnings:?}");
}

#[skuld::test]
fn the_target_of_a_running_machine_comes_from_its_record_and_the_keys(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let dir = instance_in(&fx, "foo", home);
    let keys = {
        let op = dir.op_exclusive().unwrap();
        ensure_keys(&dir, &op, &TestHost, &mut |_| {}).unwrap()
    };
    let addr: SocketAddr = "127.0.0.1:2222".parse().unwrap();
    let serial_log = home.join("serial.log");
    record_machine(&dir, &machine_at(addr, &serial_log));

    let op = dir.op_shared().unwrap();
    let target = read_target(&dir, &op).unwrap();

    assert_eq!(target.addr, addr);
    assert_eq!(target.user, SSH_USER);
    assert_eq!(target.serial_log, serial_log);
    assert_eq!(target.host_public, keys.host_public);
    assert_eq!(
        target.client_private.expose_secret(),
        keys.client_private.expose_secret()
    );
}

#[skuld::test]
fn an_instance_with_no_running_machine_is_unavailable_naming_it(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let dir = instance_in(&fx, "foo", home);
    {
        let op = dir.op_exclusive().unwrap();
        ensure_keys(&dir, &op, &TestHost, &mut |_| {}).unwrap();
    }

    let op = dir.op_shared().unwrap();
    let error = read_target(&dir, &op).expect_err("nothing runs to connect to");

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().contains("foo"), "{error}");
    assert!(error.message().contains("start"), "{error}");
}

#[skuld::test]
fn a_machine_with_no_normal_endpoint_is_an_internal_error_naming_the_instance(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let dir = instance_in(&fx, "foo", home);
    {
        let op = dir.op_exclusive().unwrap();
        ensure_keys(&dir, &op, &TestHost, &mut |_| {}).unwrap();
    }
    let mut machine = machine_at("127.0.0.1:2222".parse().unwrap(), &PathBuf::from("serial.log"));
    machine.ssh.clear();
    record_machine(&dir, &machine);

    let op = dir.op_shared().unwrap();
    let error = read_target(&dir, &op).expect_err("the machine reports no endpoint");

    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(error.message().contains("foo"), "{error}");
}
