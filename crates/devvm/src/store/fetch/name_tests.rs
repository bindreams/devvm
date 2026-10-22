use super::file_name;

const DEVICES: [&str; 24] = [
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[skuld::test]
fn windows_device_names_never_become_file_names() {
    for device in DEVICES {
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
