use super::{ALL, OPENSSH, POWERSHELL, SSH_BROKER, VIRTIO_WIN};

#[skuld::test]
fn every_pin_is_https_with_a_64_hex_sha256_and_names_what_it_is() {
    assert_eq!(ALL.len(), 4);
    for pin in ALL {
        assert!(pin.url.starts_with("https://"), "{}: {}", pin.what, pin.url);
        assert_eq!(pin.sha256.len(), 64, "{}", pin.what);
        assert!(
            pin.sha256.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{}: lower-case hex only: {}",
            pin.what,
            pin.sha256
        );
        assert!(!pin.what.trim().is_empty(), "a pin names what it is");
        assert!(!pin.url.contains(char::is_whitespace), "{}", pin.what);
    }
    let mut whats: Vec<&str> = ALL.iter().map(|pin| pin.what).collect();
    whats.sort_unstable();
    whats.dedup();
    assert_eq!(whats.len(), ALL.len(), "two pins share a name");
}

#[skuld::test]
fn a_pin_is_an_artifact_the_fetcher_takes() {
    for pin in ALL {
        let artifact = pin.artifact();
        assert_eq!(artifact.url, pin.url);
        assert_eq!(hex::encode(artifact.sha256.as_bytes()), pin.sha256);
    }
}

#[skuld::test]
fn the_pins_are_the_measured_releases() {
    assert!(VIRTIO_WIN.url.ends_with("/virtio-win-0.1.302.iso"));
    assert!(OPENSSH.url.contains("/10.4.0.0p1-Preview/OpenSSH-Win64.zip"));
    assert!(POWERSHELL.url.ends_with("/v7.7.0/PowerShell-7.7.0-win-x64.zip"));
    assert!(
        SSH_BROKER
            .url
            .ends_with("/v0.1.0/ssh-broker-0.1.0-x86_64-pc-windows-msvc.zip")
    );
}
