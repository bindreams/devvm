use std::path::Path;

use russh::keys::PrivateKey;

use crate::ErrorKind;
use crate::access::keys::InstanceKeys;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::host::{RecordingHost, TestHost};

/// The `.keys-*` directories in `dir`.
fn temporary_directories(dir: &Path) -> Vec<std::ffi::OsString> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with(".keys-"))
        .collect()
}

fn parses_as_a_pair(private: &str, public: &str) {
    let private = PrivateKey::from_openssh(private).expect("the private key parses");
    let public = russh::keys::PublicKey::from_openssh(public).expect("the public key parses");
    assert_eq!(private.public_key(), &public);
}

#[skuld::test]
fn generated_keys_round_trip_and_differ_per_instance() {
    let a = InstanceKeys::generate().unwrap();
    let b = InstanceKeys::generate().unwrap();
    parses_as_a_pair(&a.client_private, &a.client_public);
    parses_as_a_pair(&a.host_private, &a.host_public);
    assert_ne!(
        a.client_public, a.host_public,
        "the client and host keys are separate keys"
    );
    assert_ne!(a.host_public, b.host_public, "two instances never share a host key");
    assert_ne!(a.client_public, b.client_public);
}

#[skuld::test]
fn write_then_read_keys_under_a_space_and_non_ascii_directory(#[fixture(unicode_home)] home: &Path) {
    let keys = InstanceKeys::generate().unwrap();
    let dir = home.join("keys");
    keys.write(&TestHost, &dir).unwrap();
    for name in ["client", "client.pub", "host", "host.pub"] {
        assert!(dir.join(name).is_file(), "{name}");
    }
    assert_eq!(InstanceKeys::read(&dir).unwrap(), keys);
}

#[skuld::test]
fn read_of_a_missing_set_is_not_found(#[fixture(unicode_home)] home: &Path) {
    let error = InstanceKeys::read(&home.join("keys")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
}

#[skuld::test]
fn write_does_not_replace_an_existing_set(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("keys");
    let first = InstanceKeys::generate().unwrap();
    first.write(&TestHost, &dir).unwrap();
    let error = InstanceKeys::generate().unwrap().write(&TestHost, &dir).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    assert_eq!(InstanceKeys::read(&dir).unwrap(), first);
}

#[skuld::test]
fn keys_are_written_whole_or_not_at_all(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("keys");
    let keys = InstanceKeys::generate().unwrap();
    // The second private key (the host key) fails after the client key and its public half were written.
    let host = RecordingHost::new();
    host.fail_private_write(1);
    let error = keys.write(&host, &dir).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(!dir.exists(), "no half set is left at the final path");
    let leftovers = temporary_directories(home);
    assert!(
        leftovers.is_empty(),
        "no temporary directory is left behind: {leftovers:?}"
    );
    keys.write(&TestHost, &dir).unwrap();
    assert_eq!(InstanceKeys::read(&dir).unwrap(), keys);
}

#[skuld::test]
fn private_key_files_go_through_write_private_and_public_ones_do_not(#[fixture(unicode_home)] home: &Path) {
    let host = RecordingHost::new();
    InstanceKeys::generate()
        .unwrap()
        .write(&host, &home.join("keys"))
        .unwrap();
    let names: Vec<_> = host
        .private_writes()
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["client", "host"]);
}

#[skuld::test]
fn write_sweeps_the_temporary_directory_a_killed_writer_left(#[fixture(unicode_home)] home: &Path) {
    let stale = home.join(".keys-killed");
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("client"), "secret").unwrap();
    let keys = InstanceKeys::generate().unwrap();
    keys.write(&TestHost, &home.join("keys")).unwrap();
    assert!(temporary_directories(home).is_empty(), "the leftover was swept");
    assert_eq!(InstanceKeys::read(&home.join("keys")).unwrap(), keys);
}

#[skuld::test]
fn of_concurrent_writers_one_wins_and_the_rest_see_the_set_exists(#[fixture(unicode_home)] home: &Path) {
    const WRITERS: usize = 8;
    let dir = home.join("keys");
    let start = std::sync::Barrier::new(WRITERS);
    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let writers: Vec<_> = (0..WRITERS)
            .map(|_| {
                scope.spawn(|| {
                    let keys = InstanceKeys::generate().unwrap();
                    start.wait();
                    (keys.write(&TestHost, &dir), keys)
                })
            })
            .collect();
        writers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    let winners: Vec<_> = outcomes.iter().filter(|(result, _)| result.is_ok()).collect();
    assert_eq!(winners.len(), 1);
    for (result, _) in &outcomes {
        if let Err(e) = result {
            assert_eq!(e.kind(), ErrorKind::AlreadyExists, "{e}");
        }
    }
    assert_eq!(InstanceKeys::read(&dir).unwrap(), winners[0].1);
    assert!(temporary_directories(home).is_empty());
}

#[skuld::test]
fn a_leftover_that_cannot_be_removed_is_reported_and_does_not_block_the_write(#[fixture(unicode_home)] home: &Path) {
    // A regular file where a temporary directory should be: `remove_dir_all` fails on it.
    let stuck = home.join(".keys-stuck");
    std::fs::write(&stuck, "not a directory").unwrap();
    let keys = InstanceKeys::generate().unwrap();
    let warnings = keys.write(&TestHost, &home.join("keys")).unwrap();
    assert_eq!(InstanceKeys::read(&home.join("keys")).unwrap(), keys);
    assert!(stuck.exists(), "it could not be removed");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains(".keys-stuck"), "{warnings:?}");
    // The next write meets the same leftover and still goes through.
    let second = InstanceKeys::generate().unwrap();
    second.write(&TestHost, &home.join("keys2")).unwrap();
    assert_eq!(InstanceKeys::read(&home.join("keys2")).unwrap(), second);
}
