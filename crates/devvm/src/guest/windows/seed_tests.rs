use std::path::Path;

use secrecy::SecretString;

use super::seed_files;
use crate::backend::{MediaFormat, MediaKind};
use crate::guest::windows::WindowsGuest;
use crate::guest::{GuestOs, SeedKeys, SeedSpec};
use crate::instance::record::InstanceId;
use crate::media::media_tests::read_image_file;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::host::RecordingHost;

const HOST_PRIVATE: &str =
    "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\n";

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

#[skuld::test]
fn the_seed_files_hold_the_instance_keys_and_nothing_else(#[fixture(unicode_home)] home: &Path) {
    let files = seed_files(&spec(home, 1)).unwrap();
    let names: Vec<&str> = files.iter().map(|file| file.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "meta.json",
            "ssh_host_ed25519_key",
            "ssh_host_ed25519_key.pub",
            "authorized_keys"
        ]
    );
    let text =
        |name: &str| String::from_utf8(files.iter().find(|file| file.name == name).unwrap().bytes.clone()).unwrap();
    assert_eq!(text("ssh_host_ed25519_key"), HOST_PRIVATE);
    assert_eq!(text("ssh_host_ed25519_key.pub"), "ssh-ed25519 AAAAhost devvm-host\n");
    assert_eq!(text("authorized_keys"), "ssh-ed25519 AAAAclient devvm-client\n");
    let meta: serde_json::Value = serde_json::from_str(&text("meta.json")).unwrap();
    assert_eq!(meta["instance-id"], format!("devvm-{}", id(1)));
    // The private key is in its own file only.
    for file in &files {
        assert_eq!(
            String::from_utf8_lossy(&file.bytes).contains("PRIVATE KEY"),
            file.name == "ssh_host_ed25519_key",
            "{}",
            file.name
        );
    }
}

#[skuld::test]
fn a_private_key_without_a_final_newline_gets_one() {
    // OpenSSH refuses a key file whose last line is unterminated.
    let dir = std::path::PathBuf::new();
    let mut seed = spec(&dir, 1);
    seed.keys.host_private = SecretString::from(HOST_PRIVATE.trim_end());
    let files = seed_files(&seed).unwrap();
    assert_eq!(files[1].bytes, HOST_PRIVATE.as_bytes());
}

#[skuld::test]
fn the_seed_is_identical_for_the_same_instance_and_differs_between_instances(#[fixture(unicode_home)] home: &Path) {
    let first = home.join("first");
    let second = home.join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let host = RecordingHost::new();
    let image = |dir: &Path, instance: u8| {
        let media = WindowsGuest.instance_seed(&host, &spec(dir, instance)).unwrap().media;
        std::fs::read(media.path).unwrap()
    };
    assert_eq!(image(&first, 1), image(&second, 1));
    assert_ne!(image(&first, 1), image(&second, 2));
}

#[skuld::test]
fn the_seed_media_is_a_read_only_usb_disk_written_as_a_private_file(#[fixture(unicode_home)] home: &Path) {
    let host = RecordingHost::new();
    let media = WindowsGuest.instance_seed(&host, &spec(home, 1)).unwrap().media;
    assert_eq!(media.kind, MediaKind::UsbDisk);
    assert!(media.readonly);
    assert_eq!(media.format, MediaFormat::Raw);
    assert_eq!(media.path.parent(), Some(home));
    assert_eq!(host.private_writes(), std::slice::from_ref(&media.path));
    assert_eq!(
        read_image_file(&media.path, "ssh_host_ed25519_key"),
        HOST_PRIVATE.as_bytes()
    );
}

#[skuld::test]
fn a_windows_seed_with_an_empty_key_is_an_error_naming_it(#[fixture(unicode_home)] home: &Path) {
    type Blank = fn(&mut SeedKeys);
    let blanks: [(&str, Blank); 3] = [
        ("client_public", |keys| keys.client_public.clear()),
        ("host_private", |keys| keys.host_private = SecretString::from("")),
        ("host_public", |keys| keys.host_public.clear()),
    ];
    for (name, blank) in blanks {
        let mut empty = spec(home, 1);
        blank(&mut empty.keys);
        let error = seed_files(&empty)
            .err()
            .unwrap_or_else(|| panic!("a seed with an empty `{name}` was made"));
        assert_eq!(error.kind(), crate::ErrorKind::Internal, "{name}");
        assert!(error.message().contains(name), "{name}: {}", error.message());
    }
}
