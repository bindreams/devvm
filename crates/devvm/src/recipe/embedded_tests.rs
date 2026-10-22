use crate::Arch;
use crate::recipe::{Recipe, Source, embedded};

fn pins() -> (crate::recipe::Pin, crate::recipe::Pin) {
    let flat = Recipe::embedded("ubuntu-24.04").unwrap().flatten().unwrap();
    let Source::DiskImage(per_arch) = flat.root else {
        panic!("the embedded Ubuntu base is a disk image");
    };
    (
        per_arch.get(Arch::Amd64).expect("amd64 pin").clone(),
        per_arch.get(Arch::Arm64).expect("arm64 pin").clone(),
    )
}

#[skuld::test]
fn embedded_names_list_contains_ubuntu_24_04() {
    assert!(embedded::names().contains(&"ubuntu-24.04"));
}

#[skuld::test]
fn embedded_ubuntu_resolves_a_different_pin_per_arch() {
    let (amd64, arm64) = pins();
    assert!(amd64.url.ends_with("-amd64.img"), "{}", amd64.url);
    assert!(arm64.url.ends_with("-arm64.img"), "{}", arm64.url);
    assert_ne!(amd64.sha256, arm64.sha256);
}

#[skuld::test]
fn embedded_pins_are_dated_release_urls() {
    let (amd64, arm64) = pins();
    for pin in [amd64, arm64] {
        let rest = pin
            .url
            .strip_prefix("https://cloud-images.ubuntu.com/releases/noble/release-")
            .unwrap_or_else(|| panic!("not a dated noble release URL: {}", pin.url));
        let date = &rest[..8];
        assert!(date.bytes().all(|b| b.is_ascii_digit()), "{}", pin.url);
        assert_eq!(rest.as_bytes()[8], b'/', "{}", pin.url);
    }
}
