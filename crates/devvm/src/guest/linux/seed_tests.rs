use std::path::Path;

use secrecy::{ExposeSecret, SecretString};
use yaml_serde::Value;

use crate::guest::GuestOs;
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
            host_private: SecretString::from(HOST_PRIVATE),
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

/// Hostnames PyYAML (YAML 1.1, which cloud-init uses) would read as a boolean or a date if left unquoted.
const AMBIGUOUS_NAMES: [&str; 9] = ["yes", "no", "on", "off", "y", "n", "2024-01-15", "1e3", "0o17"];

#[skuld::test]
fn every_string_in_the_seed_is_quoted(#[fixture(unicode_home)] home: &Path) {
    for name in AMBIGUOUS_NAMES {
        let mut named = spec(home, 1);
        named.hostname = name.to_owned();
        let files = LinuxGuest.instance_seed(&named).unwrap();
        let media = files.media;
        let meta = String::from_utf8(read_image_file(&media.path, "meta-data")).unwrap();
        let user = String::from_utf8(read_image_file(&media.path, "user-data")).unwrap();

        // JSON is YAML, and a JSON parser accepts a scalar only if it is quoted: `yes` is no JSON value.
        let meta: serde_json::Value = serde_json::from_str(&meta).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(meta["local-hostname"].as_str(), Some(name));
        let (header, body) = user.split_once('\n').unwrap();
        assert_eq!(header, "#cloud-config");
        let user: serde_json::Value = serde_json::from_str(body).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(user["hostname"].as_str(), Some(name));
        assert_eq!(user["users"][0]["name"].as_str(), Some("user"));
        assert_eq!(user["ssh_pwauth"].as_bool(), Some(false), "booleans stay booleans");
        assert_eq!(user["ssh_keys"]["ed25519_private"].as_str(), Some(HOST_PRIVATE));
    }
}

/// Characters PyYAML (YAML 1.1) rejects (`ReaderError`) or reads as line breaks (NEL, LS, PS) when written raw in a double-quoted
/// scalar. The exhaustive PyYAML test (`seed_pyyaml_tests.rs`) pins the class; this one runs everywhere.
const UNREADABLE_RAW: [char; 9] = [
    '\u{7f}', '\u{80}', '\u{85}', '\u{9f}', '\u{fffe}', '\u{ffff}', '\u{8d}', '\u{2028}', '\u{2029}',
];

#[skuld::test]
fn characters_yaml_cannot_carry_raw_are_escaped_in_every_key(#[fixture(unicode_home)] home: &Path) {
    for c in UNREADABLE_RAW {
        let mut keyed = spec(home, 1);
        keyed.keys.client_public = format!("ssh-ed25519 AAAAclient a{c}b");
        keyed.keys.host_private = SecretString::from(format!("{HOST_PRIVATE}{c}"));
        keyed.keys.host_public = format!("{c}ssh-ed25519 AAAAhost");
        let media = LinuxGuest.instance_seed(&keyed).unwrap().media;
        let user = String::from_utf8(read_image_file(&media.path, "user-data")).unwrap();

        assert!(!user.contains(c), "U+{:04X} is written raw", c as u32);
        assert!(
            user.contains(&format!("\\u{:04x}", c as u32)),
            "U+{:04X}: {user}",
            c as u32
        );

        // JSON is YAML: the same text must read back as the input through both parsers.
        let body = user.split_once('\n').unwrap().1;
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        let yaml: Value = yaml_serde::from_str(&user).unwrap();
        assert_eq!(
            json["users"][0]["ssh_authorized_keys"][0].as_str(),
            Some(keyed.keys.client_public.as_str())
        );
        assert_eq!(
            field(&field(&yaml, "users").as_sequence().unwrap()[0], "ssh_authorized_keys")[0].as_str(),
            Some(keyed.keys.client_public.as_str())
        );
        for (parsed, expected) in [
            (
                &json["ssh_keys"]["ed25519_private"],
                keyed.keys.host_private.expose_secret(),
            ),
            (&json["ssh_keys"]["ed25519_public"], keyed.keys.host_public.as_str()),
        ] {
            assert_eq!(parsed.as_str(), Some(expected));
        }
        let host_keys = field(&yaml, "ssh_keys");
        assert_eq!(
            field(host_keys, "ed25519_private").as_str(),
            Some(keyed.keys.host_private.expose_secret())
        );
        assert_eq!(
            field(host_keys, "ed25519_public").as_str(),
            Some(keyed.keys.host_public.as_str())
        );
    }
}

#[skuld::test]
fn printable_non_ascii_stays_raw(#[fixture(unicode_home)] home: &Path) {
    let mut keyed = spec(home, 1);
    keyed.keys.client_public = "ssh-ed25519 AAAAclient \u{e9}\u{416}\u{4e16}\u{1f600}\u{a0}".to_owned();
    let media = LinuxGuest.instance_seed(&keyed).unwrap().media;
    let user = String::from_utf8(read_image_file(&media.path, "user-data")).unwrap();
    assert!(user.contains(&keyed.keys.client_public), "{user}");
}
