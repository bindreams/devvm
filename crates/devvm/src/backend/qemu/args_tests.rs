use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{LaunchPaths, build_args, escape_option_value, guest_port};
use crate::backend::fake::fake_spec;
use crate::backend::{DeviceProfile, DiskBus, Endpoint, Firmware, MachineSpec, Media, MediaKind, NetModel};
use crate::contain::Lifetime;
use crate::host::Accel;
use crate::test_support::fixtures::UNICODE_DIR;
use crate::{Arch, ErrorKind, Network};

fn spec() -> MachineSpec {
    let mut spec = fake_spec(
        PathBuf::from("inst/disk.img"),
        PathBuf::from("inst/serial.log"),
        Lifetime::OutlivesOwner,
    );
    spec.accel = Accel::Whpx;
    spec.cpus = 2;
    spec.memory = 2048 << 20;
    spec.media = vec![Media {
        path: PathBuf::from("inst/seed/seed.iso"),
        kind: MediaKind::Cdrom,
        boot_index: None,
        readonly: true,
    }];
    spec
}

fn paths() -> LaunchPaths {
    LaunchPaths {
        qmp: PathBuf::from("tmp/q.sock"),
        code: Some(PathBuf::from("share/edk2-x86_64-code.fd")),
        vars: Some(PathBuf::from("inst/vars.fd")),
    }
}

fn args(spec: &MachineSpec, paths: &LaunchPaths) -> Vec<String> {
    build_args(spec, paths)
        .unwrap()
        .into_iter()
        .map(|arg| arg.into_string().unwrap())
        .collect()
}

#[skuld::test]
fn amd64_whpx_command_line_matches_the_measured_one() {
    #[rustfmt::skip]
    let expected = [
        "-accel", "whpx", "-machine", "q35", "-cpu", "max", "-smp", "2", "-m", "2048", "-display", "none",
        "-nodefaults", "-no-user-config", "-rtc", "base=utc",
        "-chardev", "socket,id=qmp,path=tmp/q.sock,server=on,wait=off", "-object", "monitor-qmp,id=qmp0,chardev=qmp",
        "-chardev", "file,id=ser0,path=inst/serial.log", "-serial", "chardev:ser0",
        "-drive", "if=pflash,format=raw,readonly=on,file=share/edk2-x86_64-code.fd",
        "-drive", "if=pflash,format=raw,file=inst/vars.fd",
        "-drive", "if=none,id=d0,file=inst/disk.img,format=qcow2", "-device", "virtio-blk-pci,drive=d0,bootindex=1",
        "-drive", "if=none,id=m0,file=inst/seed/seed.iso,media=cdrom,readonly=on", "-device", "ide-cd,drive=m0",
        "-netdev", "user,id=n0,hostfwd=tcp:127.0.0.1:0-:22", "-device", "virtio-net-pci,netdev=n0",
    ];
    assert_eq!(args(&spec(), &paths()), expected);
}

#[skuld::test]
fn arm64_tcg_command_line_is_built_with_the_virt_machine_and_its_firmware() {
    let mut spec = spec();
    spec.arch = Arch::Arm64;
    spec.accel = Accel::Tcg;
    let mut paths = paths();
    paths.code = Some(PathBuf::from("share/edk2-aarch64-code.fd"));

    let args = args(&spec, &paths);

    assert_eq!(&args[..4], ["-accel", "tcg", "-machine", "virt,gic-version=3"]);
    assert!(args.contains(&"if=pflash,format=raw,readonly=on,file=share/edk2-aarch64-code.fd".to_owned()));
}

#[skuld::test]
fn an_arm64_guest_under_whpx_is_refused() {
    let mut spec = spec();
    spec.arch = Arch::Arm64;
    let error = build_args(&spec, &paths()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotSupported, "{error}");
}

#[skuld::test]
fn accelerators_are_named_for_qemu() {
    for (accel, name) in [
        (Accel::Whpx, "whpx"),
        (Accel::Kvm, "kvm"),
        (Accel::Hvf, "hvf"),
        (Accel::Tcg, "tcg"),
    ] {
        let mut spec = spec();
        spec.accel = accel;
        assert_eq!(args(&spec, &paths())[..2], ["-accel", name]);
    }
}

#[skuld::test]
fn network_none_adds_restrict_and_nat_does_not() {
    let mut spec = spec();
    spec.network = Network::None;
    assert!(args(&spec, &paths()).contains(&"user,id=n0,hostfwd=tcp:127.0.0.1:0-:22,restrict=on".to_owned()));

    spec.network = Network::Nat;
    let nat = args(&spec, &paths());
    assert!(!nat.iter().any(|arg| arg.contains("restrict")), "{nat:?}");
}

#[skuld::test]
fn forwards_bind_loopback_and_use_port_zero() {
    let args = args(&spec(), &paths());
    let netdev = args.iter().find(|arg| arg.starts_with("user,")).unwrap();
    assert!(netdev.contains("hostfwd=tcp:127.0.0.1:0-:22"), "{netdev}");
    assert!(!netdev.contains("0.0.0.0"), "{netdev}");
}

#[skuld::test]
fn no_forward_means_no_hostfwd() {
    let mut spec = spec();
    spec.forwards.clear();
    let args = args(&spec, &paths());
    assert!(args.contains(&"user,id=n0".to_owned()), "{args:?}");
}

#[skuld::test]
fn elevated_endpoint_is_not_supported_yet() {
    assert_eq!(guest_port(Endpoint::Normal).unwrap(), 22);
    let error = guest_port(Endpoint::Elevated).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotSupported, "{error}");
    let mut spec = spec();
    spec.forwards = vec![Endpoint::Normal, Endpoint::Elevated];
    assert_eq!(build_args(&spec, &paths()).unwrap_err().kind(), ErrorKind::NotSupported);
}

#[skuld::test]
fn media_follow_boot_index_and_the_seed_is_read_only() {
    let mut spec = spec();
    spec.media = vec![
        Media {
            path: PathBuf::from("inst/data.img"),
            kind: MediaKind::Disk,
            boot_index: None,
            readonly: false,
        },
        Media {
            path: PathBuf::from("isos/win.iso"),
            kind: MediaKind::Cdrom,
            boot_index: Some(1),
            readonly: true,
        },
        Media {
            path: PathBuf::from("inst/seed.img"),
            kind: MediaKind::Disk,
            boot_index: Some(2),
            readonly: true,
        },
    ];

    let args = args(&spec, &paths());

    let media: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| arg.starts_with("if=none,id=m"))
        .collect();
    assert_eq!(
        media,
        [
            "if=none,id=m0,file=isos/win.iso,media=cdrom,readonly=on",
            "if=none,id=m1,file=inst/seed.img,format=raw,readonly=on",
            "if=none,id=m2,file=inst/data.img,format=raw",
        ],
        "boot order first; a medium with no boot index last"
    );
    assert!(args.contains(&"ide-cd,drive=m0,bootindex=1".to_owned()));
    assert!(args.contains(&"virtio-blk-pci,drive=m1,bootindex=2".to_owned()));
    assert!(args.contains(&"virtio-blk-pci,drive=m2".to_owned()));
    // The system disk comes after every medium that wants to boot.
    assert!(
        args.contains(&"virtio-blk-pci,drive=d0,bootindex=3".to_owned()),
        "{args:?}"
    );
}

#[skuld::test]
fn a_disk_kind_medium_is_a_readonly_virtio_blk_and_a_cdrom_is_ide_cd() {
    let mut spec = spec();
    spec.media = vec![
        Media {
            path: PathBuf::from("seed.img"),
            kind: MediaKind::Disk,
            boot_index: None,
            readonly: true,
        },
        Media {
            path: PathBuf::from("seed.iso"),
            kind: MediaKind::Cdrom,
            boot_index: None,
            readonly: true,
        },
    ];
    let args = args(&spec, &paths());
    assert!(
        args.contains(&"if=none,id=m0,file=seed.img,format=raw,readonly=on".to_owned()),
        "{args:?}"
    );
    assert!(args.contains(&"virtio-blk-pci,drive=m0".to_owned()));
    assert!(args.contains(&"if=none,id=m1,file=seed.iso,media=cdrom,readonly=on".to_owned()));
    assert!(args.contains(&"ide-cd,drive=m1".to_owned()));
}

#[skuld::test]
fn device_profiles_select_buses_and_models() {
    let mut spec = spec();
    spec.profile = DeviceProfile {
        disk: DiskBus::Sata,
        net: NetModel::E1000,
        no_reboot: false,
    };
    let sata = args(&spec, &paths());
    assert!(sata.contains(&"ich9-ahci,id=ahci".to_owned()), "{sata:?}");
    assert!(
        sata.contains(&"ide-hd,drive=d0,bus=ahci.0,bootindex=1".to_owned()),
        "{sata:?}"
    );
    assert!(sata.contains(&"e1000,netdev=n0".to_owned()));
    assert!(!sata.iter().any(|arg| arg.starts_with("virtio-")));

    spec.profile.disk = DiskBus::Nvme;
    let nvme = args(&spec, &paths());
    assert!(
        nvme.contains(&"nvme,drive=d0,serial=devvm,bootindex=1".to_owned()),
        "{nvme:?}"
    );
}

#[skuld::test]
fn no_reboot_follows_the_device_profile() {
    let mut spec = spec();
    spec.profile.no_reboot = true;
    assert!(args(&spec, &paths()).contains(&"-no-reboot".to_owned()));
    spec.profile.no_reboot = false;
    assert!(!args(&spec, &paths()).contains(&"-no-reboot".to_owned()));
}

#[skuld::test]
fn qmp_uses_monitor_qmp_not_mon() {
    let args = args(&spec(), &paths());
    assert!(args.contains(&"monitor-qmp,id=qmp0,chardev=qmp".to_owned()));
    assert!(!args.contains(&"-mon".to_owned()));
    assert!(!args.contains(&"-qmp".to_owned()), "never -qmp stdio");
}

#[skuld::test]
fn a_comma_in_a_path_is_doubled() {
    assert_eq!(escape_option_value(Path::new("a,b,,c")).unwrap(), "a,,b,,,,c");
    let mut spec = spec();
    spec.disk = PathBuf::from("in,st/disk.img");
    spec.serial_log = PathBuf::from("in,st/serial.log");
    let mut paths = paths();
    paths.qmp = PathBuf::from("t,mp/q.sock");
    let args = args(&spec, &paths);
    assert!(
        args.contains(&"if=none,id=d0,file=in,,st/disk.img,format=qcow2".to_owned()),
        "{args:?}"
    );
    assert!(args.contains(&"file,id=ser0,path=in,,st/serial.log".to_owned()));
    assert!(args.contains(&"socket,id=qmp,path=t,,mp/q.sock,server=on,wait=off".to_owned()));
}

#[skuld::test]
fn paths_with_a_space_and_non_ascii_are_single_argv_items() {
    let disk = PathBuf::from("base").join(UNICODE_DIR).join("inst").join("disk.img");
    let mut spec = spec();
    spec.disk = disk.clone();
    let args = build_args(&spec, &paths()).unwrap();
    let wanted: OsString = format!("if=none,id=d0,file={},format=qcow2", disk.display()).into();
    assert!(args.contains(&wanted), "{args:?}");
    assert!(wanted.to_string_lossy().contains(' '), "the item holds the spaces itself");
}

#[skuld::test]
fn a_path_that_is_not_unicode_is_refused() {
    #[cfg(windows)]
    let path = {
        use std::os::windows::ffi::OsStringExt;
        PathBuf::from(OsString::from_wide(&[0x61, 0xD800, 0x62]))
    };
    #[cfg(not(windows))]
    let path = {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(OsString::from_vec(vec![0x61, 0xFF, 0x62]))
    };
    assert_eq!(escape_option_value(&path).unwrap_err().kind(), ErrorKind::Unavailable);
}

#[skuld::test]
fn an_empty_firmware_code_path_is_filled_from_share_and_vars_from_the_disk_directory() {
    let mut spec = spec();
    spec.disk = PathBuf::from("inst").join("disk.img");
    let qmp = PathBuf::from("tmp/q.sock");

    // No firmware named: the defaults.
    let resolved = LaunchPaths::resolve(&spec, qmp.clone(), Path::new("share"));
    assert_eq!(resolved.code, Some(Path::new("share").join("edk2-x86_64-code.fd")));
    assert_eq!(resolved.vars, Some(PathBuf::from("inst").join("vars.fd")));

    // An empty code path counts as "not named"; a named vars file is kept.
    spec.firmware = Some(Firmware {
        code: PathBuf::new(),
        vars: Some(PathBuf::from("other/vars.fd")),
    });
    let resolved = LaunchPaths::resolve(&spec, qmp.clone(), Path::new("share"));
    assert_eq!(resolved.code, Some(Path::new("share").join("edk2-x86_64-code.fd")));
    assert_eq!(resolved.vars, Some(PathBuf::from("other/vars.fd")));

    // A named code path wins.
    spec.firmware = Some(Firmware {
        code: PathBuf::from("mine/code.fd"),
        vars: None,
    });
    let resolved = LaunchPaths::resolve(&spec, qmp, Path::new("share"));
    assert_eq!(resolved.code, Some(PathBuf::from("mine/code.fd")));
    assert_eq!(resolved.vars, Some(PathBuf::from("inst").join("vars.fd")));

    // The arm64 defaults.
    spec.arch = Arch::Arm64;
    spec.firmware = None;
    let resolved = LaunchPaths::resolve(&spec, PathBuf::new(), Path::new("share"));
    assert_eq!(resolved.code, Some(Path::new("share").join("edk2-aarch64-code.fd")));
    assert_eq!(resolved.vars, Some(PathBuf::from("inst").join("vars.fd")));
}

#[skuld::test]
fn a_machine_without_firmware_has_no_pflash_drives() {
    let paths = LaunchPaths {
        qmp: PathBuf::from("tmp/q.sock"),
        code: None,
        vars: None,
    };
    let args = args(&spec(), &paths);
    assert!(!args.iter().any(|arg| arg.contains("pflash")), "{args:?}");
}

#[skuld::test]
fn cpus_and_memory_are_passed_through() {
    let mut spec = spec();
    spec.cpus = 6;
    spec.memory = 4096 << 20;
    let args = args(&spec, &paths());
    assert_eq!(args[6..10], ["-smp", "6", "-m", "4096"]);
}
