use super::file_name;
use crate::test_support::fixtures::WINDOWS_DEVICE_NAMES;

#[skuld::test]
fn windows_device_names_never_become_file_names() {
    for device in WINDOWS_DEVICE_NAMES {
        let encoded = device.replace('$', "%24");
        for url in [
            format!("https://h/{encoded}"),
            format!("https://h/{}", encoded.to_ascii_lowercase()),
            format!("https://h/{encoded}.txt"),
            format!("https://h/{encoded}.tar.gz?token=x"),
        ] {
            assert_eq!(file_name(&url), "artifact", "{url}");
        }
    }
}

#[skuld::test]
fn names_that_only_resemble_devices_are_kept() {
    for (url, name) in [
        ("https://h/COM10.img", "COM10.img"),
        ("https://h/COM0", "COM0"),
        ("https://h/CONSOLE.img", "CONSOLE.img"),
        ("https://h/LPT.img", "LPT.img"),
        ("https://h/disk.img", "disk.img"),
    ] {
        assert_eq!(file_name(url), name, "{url}");
    }
}
