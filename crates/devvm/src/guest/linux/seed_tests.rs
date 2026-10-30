use std::path::Path;

use yaml_serde::Value;

use crate::guest::linux::LinuxGuest;
use crate::guest::{SeedKeys, SeedSpec};
use crate::instance::record::InstanceId;
use crate::media::media_tests::read_image_file;
use crate::test_support::fixtures::unicode_home;

const HOST_PRIVATE: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\nAAAA: tricky # not a comment\n-----END OPENSSH PRIVATE KEY-----\n";

fn id(byte: u8) -> InstanceId {
    hex::encode([byte; 16]).parse().unwrap()
}

fn spec(dir: &Path, instance: u8) -> SeedSpec {
    SeedSpec {
        hostname: "dvm-test".to_owned(),
        dir: dir.to_path_buf(),
        instance_id: id(instance),
        keys: SeedKeys {
            client_public: "ssh-ed25519 AAAAclient devvm-client".to_owned(),
            host_private: HOST_PRIVATE.to_owned(),
            host_public: "ssh-ed25519 AAAAhost devvm-host".to_owned(),
        },
    }
}

fn seed_file(dir: &Path, instance: u8, name: &str) -> String {
    let media = LinuxGuest.instance_seed(&spec(dir, instance)).unwrap().media;
    String::from_utf8(read_image_file(&media.path, name)).unwrap()
}

fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value
        .get(key)
        .unwrap_or_else(|| panic!("`{key}` is missing in {value:?}"))
}

#[skuld::test]
fn seed_user_data_matches_the_measured_shape(#[fixture(unicode_home)] home: &Path) {
    let text = seed_file(home, 1, "user-data");
    assert_eq!(text.lines().next(), Some("#cloud-config"));

    let data: Value = yaml_serde::from_str(&text).unwrap();
    assert_eq!(field(&data, "hostname").as_str(), Some("dvm-test"));
    assert_eq!(field(&data, "ssh_pwauth").as_bool(), Some(false));

    let users = field(&data, "users").as_sequence().unwrap();
    assert_eq!(users.len(), 1);
    let user = &users[0];
    assert_eq!(field(user, "name").as_str(), Some("user"));
    assert_eq!(field(user, "shell").as_str(), Some("/bin/bash"));
    assert_eq!(field(user, "sudo").as_str(), Some("ALL=(ALL) NOPASSWD:ALL"));
    let authorized: Vec<&str> = field(user, "ssh_authorized_keys")
        .as_sequence()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(authorized, ["ssh-ed25519 AAAAclient devvm-client"]);

    let host_keys = field(&data, "ssh_keys");
    assert_eq!(
        field(host_keys, "ed25519_private").as_str(),
        Some(HOST_PRIVATE),
        "the private key survives byte for byte, trailing newline included"
    );
    assert_eq!(
        field(host_keys, "ed25519_public").as_str(),
        Some("ssh-ed25519 AAAAhost devvm-host")
    );
}

#[skuld::test]
fn instance_id_is_the_instance_and_stable_across_calls(#[fixture(unicode_home)] home: &Path) {
    let first = home.join("first");
    let second = home.join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();

    let meta: Value = yaml_serde::from_str(&seed_file(&first, 1, "meta-data")).unwrap();
    assert_eq!(
        field(&meta, "instance-id").as_str(),
        Some(format!("devvm-{}", id(1)).as_str())
    );
    assert_eq!(field(&meta, "local-hostname").as_str(), Some("dvm-test"));

    assert_eq!(seed_file(&second, 1, "meta-data"), seed_file(&first, 1, "meta-data"));
    assert_ne!(seed_file(&second, 2, "meta-data"), seed_file(&first, 1, "meta-data"));
}

#[skuld::test]
fn the_seed_is_byte_identical_for_equal_input(#[fixture(unicode_home)] home: &Path) {
    let first = home.join("first");
    let second = home.join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();

    let one = LinuxGuest.instance_seed(&spec(&first, 1)).unwrap().media;
    let two = LinuxGuest.instance_seed(&spec(&second, 1)).unwrap().media;
    assert_eq!(std::fs::read(&one.path).unwrap(), std::fs::read(&two.path).unwrap());

    let mut other_keys = spec(&second, 1);
    other_keys.keys.client_public = "ssh-ed25519 AAAAother devvm-client".to_owned();
    let three = LinuxGuest.instance_seed(&other_keys).unwrap().media;
    assert_ne!(std::fs::read(&one.path).unwrap(), std::fs::read(&three.path).unwrap());
}

#[skuld::test]
fn the_seed_media_is_a_readonly_disk_in_the_scratch_directory(#[fixture(unicode_home)] home: &Path) {
    let media = LinuxGuest.instance_seed(&spec(home, 1)).unwrap().media;
    assert!(media.path.starts_with(home));
    assert_eq!(media.kind, crate::backend::MediaKind::Disk);
    assert!(media.readonly);
}
