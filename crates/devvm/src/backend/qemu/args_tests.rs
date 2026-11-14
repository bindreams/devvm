use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{AHCI_PORTS, LaunchPaths, build_args, escape_option_value, guest_port};
use crate::backend::fake::fake_spec;
use crate::backend::{
    DeviceProfile, DiskBus, Endpoint, Firmware, MachineSpec, Media, MediaFormat, MediaKind, NetModel, Video,
};
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
        format: MediaFormat::Raw,
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

/// What follows each `flag` in `args`.
fn values_of<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
    args.windows(2)
        .filter(|pair| pair[0] == flag)
        .map(|pair| pair[1].as_str())
        .collect()
}

fn args(spec: &MachineSpec, paths: &LaunchPaths) -> Vec<String> {
    build_args(spec, paths)
        .unwrap()
        .into_iter()
        .map(|arg| arg.into_string().unwrap())
        .collect()
}

/// The command line `spec` and `paths` must produce, written out from their values: the arguments in the order QEMU
/// is given them, with the machine type and accelerator the caller names.
fn expected_args(spec: &MachineSpec, paths: &LaunchPaths, accel: &str, machine: &str) -> Vec<String> {
    let qmp = paths.qmp.display();
    let log = spec.serial_log.display();
    let code = paths.code.as_ref().unwrap().display();
    let vars = paths.vars.as_ref().unwrap().display();
    let disk = spec.disk.display();
    let seed = spec.media[0].path.display();
    let mut expected: Vec<String> = [
        "-accel",
        accel,
        "-machine",
        machine,
        "-cpu",
        "max",
        "-smp",
        &spec.cpus.to_string(),
        "-m",
        &(spec.memory >> 20).to_string(),
        "-display",
        "none",
        "-nodefaults",
        "-no-user-config",
        "-rtc",
        "base=utc",
    ]
    .map(str::to_owned)
    .to_vec();
    for (flag, value) in [
        ("-chardev", format!("socket,id=qmp,path={qmp},server=on,wait=off")),
        ("-object", "monitor-qmp,id=qmp0,chardev=qmp".to_owned()),
        ("-chardev", format!("file,id=ser0,path={log}")),
        ("-serial", "chardev:ser0".to_owned()),
        ("-drive", format!("if=pflash,format=raw,readonly=on,file={code}")),
        ("-drive", format!("if=pflash,format=raw,file={vars}")),
        ("-drive", format!("if=none,id=d0,file={disk},format=qcow2")),
        ("-device", "virtio-blk-pci,drive=d0,bootindex=1".to_owned()),
        (
            "-drive",
            format!("if=none,id=m0,file={seed},format=raw,media=cdrom,readonly=on"),
        ),
        ("-device", "ide-cd,drive=m0,bus=ide.0".to_owned()),
        ("-netdev", "user,id=n0,hostfwd=tcp:127.0.0.1:0-:22".to_owned()),
        ("-device", "virtio-net-pci,netdev=n0".to_owned()),
    ] {
        expected.push(flag.to_owned());
        expected.push(value);
    }
    expected
}

#[skuld::test]
fn amd64_whpx_command_line_is_the_one_the_spec_asks_for() {
    let (spec, paths) = (spec(), paths());
    assert_eq!(args(&spec, &paths), expected_args(&spec, &paths, "whpx", "q35"));
}

#[skuld::test]
fn arm64_tcg_command_line_is_built_with_the_virt_machine_and_its_firmware() {
    let mut spec = spec();
    spec.arch = Arch::Arm64;
    spec.accel = Accel::Tcg;
    let mut paths = paths();
    paths.code = Some(PathBuf::from("share/edk2-aarch64-code.fd"));

    assert_eq!(
        args(&spec, &paths),
        expected_args(&spec, &paths, "tcg", "virt,gic-version=3")
    );
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
        assert_eq!(values_of(&args(&spec, &paths()), "-accel"), [name]);
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
            format: MediaFormat::Raw,
        },
        Media {
            path: PathBuf::from("isos/win.iso"),
            kind: MediaKind::Cdrom,
            boot_index: Some(1),
            readonly: true,
            format: MediaFormat::Raw,
        },
        Media {
            path: PathBuf::from("inst/seed.img"),
            kind: MediaKind::Disk,
            boot_index: Some(2),
            readonly: true,
            format: MediaFormat::Raw,
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
            "if=none,id=m0,file=isos/win.iso,format=raw,media=cdrom,readonly=on",
            "if=none,id=m1,file=inst/seed.img,format=raw,readonly=on",
            "if=none,id=m2,file=inst/data.img,format=raw",
        ],
        "boot order first; a medium with no boot index last"
    );
    assert!(args.contains(&"ide-cd,drive=m0,bus=ide.0,bootindex=1".to_owned()));
    assert!(args.contains(&"virtio-blk-pci,drive=m1,bootindex=2".to_owned()));
    assert!(args.contains(&"virtio-blk-pci,drive=m2".to_owned()));
    // The system disk comes after every medium that wants to boot.
    assert!(
        args.contains(&"virtio-blk-pci,drive=d0,bootindex=3".to_owned()),
        "{args:?}"
    );
}

#[skuld::test]
fn a_disk_kind_medium_is_a_readonly_virtio_blk_and_a_cdrom_is_ide_cd_on_the_first_ahci_port() {
    let mut spec = spec();
    spec.media = vec![
        Media {
            path: PathBuf::from("seed.img"),
            kind: MediaKind::Disk,
            boot_index: None,
            readonly: true,
            format: MediaFormat::Raw,
        },
        Media {
            path: PathBuf::from("seed.iso"),
            kind: MediaKind::Cdrom,
            boot_index: None,
            readonly: true,
            format: MediaFormat::Raw,
        },
    ];
    let args = args(&spec, &paths());
    assert!(
        args.contains(&"if=none,id=m0,file=seed.img,format=raw,readonly=on".to_owned()),
        "{args:?}"
    );
    assert!(args.contains(&"virtio-blk-pci,drive=m0".to_owned()));
    assert!(args.contains(&"if=none,id=m1,file=seed.iso,format=raw,media=cdrom,readonly=on".to_owned()));
    assert!(args.contains(&"ide-cd,drive=m1,bus=ide.0".to_owned()));
}

#[skuld::test]
fn device_profiles_select_buses_and_models() {
    let mut spec = spec();
    spec.profile = DeviceProfile {
        disk: DiskBus::Sata,
        net: NetModel::E1000,
        video: Video::None,
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
fn a_comma_in_a_firmware_or_media_path_is_doubled() {
    let mut spec = spec();
    spec.media = vec![
        Media {
            path: PathBuf::from("se,ed/seed.iso"),
            kind: MediaKind::Cdrom,
            boot_index: None,
            readonly: true,
            format: MediaFormat::Raw,
        },
        Media {
            path: PathBuf::from("da,ta/data.img"),
            kind: MediaKind::Disk,
            boot_index: None,
            readonly: false,
            format: MediaFormat::Raw,
        },
    ];
    let mut paths = paths();
    paths.code = Some(PathBuf::from("sh,are/code.fd"));
    paths.vars = Some(PathBuf::from("in,st/vars.fd"));

    let args = args(&spec, &paths);

    for wanted in [
        "if=pflash,format=raw,readonly=on,file=sh,,are/code.fd",
        "if=pflash,format=raw,file=in,,st/vars.fd",
        "if=none,id=m0,file=se,,ed/seed.iso,format=raw,media=cdrom,readonly=on",
        "if=none,id=m1,file=da,,ta/data.img,format=raw",
    ] {
        assert!(args.contains(&wanted.to_owned()), "{wanted} is missing from {args:?}");
    }
}

#[skuld::test]
fn a_cdrom_is_opened_with_the_format_named_so_its_content_cannot_pick_it() {
    let args = args(&spec(), &paths());
    let drive = args.iter().find(|arg| arg.contains("media=cdrom")).unwrap();
    assert!(drive.contains(",format=raw,"), "{drive}");
}

#[skuld::test]
fn a_qcow2_cdrom_is_opened_as_qcow2() {
    let mut spec = spec();
    spec.media[0].format = MediaFormat::Qcow2;
    let args = args(&spec, &paths());
    let drive = args.iter().find(|arg| arg.contains("media=cdrom")).unwrap();
    assert_eq!(
        *drive,
        "if=none,id=m0,file=inst/seed/seed.iso,format=qcow2,media=cdrom,readonly=on"
    );
}

#[skuld::test]
fn every_medium_kind_names_its_format() {
    let mut spec = spec();
    spec.media = [MediaKind::Cdrom, MediaKind::Disk, MediaKind::UsbDisk]
        .into_iter()
        .enumerate()
        .map(|(i, kind)| Media {
            path: PathBuf::from(format!("m{i}.img")),
            kind,
            boot_index: None,
            readonly: true,
            format: MediaFormat::Qcow2,
        })
        .collect();
    let args = args(&spec, &paths());
    let drives: Vec<&str> = values_of(&args, "-drive")
        .into_iter()
        .filter(|d| d.starts_with("if=none,id=m"))
        .collect();
    assert_eq!(drives.len(), 3);
    assert!(drives.iter().all(|drive| drive.contains(",format=qcow2")), "{drives:?}");
}

#[skuld::test]
fn the_system_disk_boots_after_the_highest_boot_index() {
    let mut spec = spec();
    // The largest index that leaves room for the system disk after it.
    spec.media = vec![Media {
        path: PathBuf::from("a.img"),
        kind: MediaKind::Disk,
        boot_index: Some(i32::MAX as u32 - 1),
        readonly: true,
        format: MediaFormat::Raw,
    }];
    let args = args(&spec, &paths());
    assert!(
        args.contains(&format!("virtio-blk-pci,drive=d0,bootindex={}", i32::MAX)),
        "{args:?}"
    );
}

#[skuld::test]
fn a_boot_index_that_leaves_no_room_for_the_system_disk_is_refused_naming_the_medium() {
    for index in [i32::MAX as u32, i32::MAX as u32 + 1, u32::MAX] {
        let mut spec = spec();
        spec.media = vec![Media {
            path: PathBuf::from("big.iso"),
            kind: MediaKind::Cdrom,
            boot_index: Some(index),
            readonly: true,
            format: MediaFormat::Raw,
        }];

        let error = build_args(&spec, &paths()).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Usage, "{index}: {error}");
        assert!(error.message().contains("big.iso"), "{index}: {error}");
        assert!(error.message().contains(&index.to_string()), "{index}: {error}");
    }
}

#[skuld::test]
fn paths_with_a_space_and_non_ascii_are_single_argv_items() {
    let disk = PathBuf::from("base").join(UNICODE_DIR).join("inst").join("disk.img");
    let mut spec = spec();
    spec.disk = disk.clone();
    let args = build_args(&spec, &paths()).unwrap();
    let wanted: OsString = format!("if=none,id=d0,file={},format=qcow2", disk.display()).into();
    assert!(args.contains(&wanted), "{args:?}");
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
    let resolved = LaunchPaths::resolve(&spec, qmp.clone(), Path::new("share")).unwrap();
    assert_eq!(resolved.code, Some(Path::new("share").join("edk2-x86_64-code.fd")));
    assert_eq!(resolved.vars, Some(PathBuf::from("inst").join("vars.fd")));

    // An empty code path counts as "not named"; a named vars file is kept.
    spec.firmware = Some(Firmware {
        code: PathBuf::new(),
        vars: Some(PathBuf::from("other/vars.fd")),
    });
    let resolved = LaunchPaths::resolve(&spec, qmp.clone(), Path::new("share")).unwrap();
    assert_eq!(resolved.code, Some(Path::new("share").join("edk2-x86_64-code.fd")));
    assert_eq!(resolved.vars, Some(PathBuf::from("other/vars.fd")));

    // A named code path wins.
    spec.firmware = Some(Firmware {
        code: PathBuf::from("mine/code.fd"),
        vars: None,
    });
    let resolved = LaunchPaths::resolve(&spec, qmp, Path::new("share")).unwrap();
    assert_eq!(resolved.code, Some(PathBuf::from("mine/code.fd")));
    assert_eq!(resolved.vars, Some(PathBuf::from("inst").join("vars.fd")));

    // The arm64 defaults.
    spec.arch = Arch::Arm64;
    spec.firmware = None;
    let resolved = LaunchPaths::resolve(&spec, PathBuf::new(), Path::new("share")).unwrap();
    assert_eq!(resolved.code, Some(Path::new("share").join("edk2-aarch64-code.fd")));
    assert_eq!(resolved.vars, Some(PathBuf::from("inst").join("vars.fd")));
}

#[skuld::test]
fn a_disk_with_no_directory_has_nowhere_for_its_firmware_variables_and_is_refused() {
    let mut spec = spec();
    spec.disk = PathBuf::new();

    let error = LaunchPaths::resolve(&spec, PathBuf::new(), Path::new("share")).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(error.message().contains("disk"), "{error}");

    // Variables the spec names need no directory of the disk.
    spec.firmware = Some(Firmware {
        code: PathBuf::new(),
        vars: Some(PathBuf::from("other/vars.fd")),
    });
    assert!(LaunchPaths::resolve(&spec, PathBuf::new(), Path::new("share")).is_ok());
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
    assert_eq!(values_of(&args, "-smp"), ["6"]);
    assert_eq!(values_of(&args, "-m"), ["4096"]);
}

fn usb(path: &str, boot_index: Option<u32>, readonly: bool) -> Media {
    Media {
        path: PathBuf::from(path),
        kind: MediaKind::UsbDisk,
        boot_index,
        readonly,
        format: MediaFormat::Raw,
    }
}

#[skuld::test]
fn usb_media_are_removable_usb_storage_on_one_xhci_controller() {
    let mut spec = spec();
    spec.media = vec![usb("inst/answer.img", None, false), usb("inst/seed.img", None, true)];

    let args = args(&spec, &paths());

    let devices = values_of(&args, "-device");
    assert_eq!(
        devices.iter().filter(|device| device.starts_with("qemu-xhci")).count(),
        1,
        "{devices:?}"
    );
    assert!(devices.contains(&"qemu-xhci,id=xhci"), "{devices:?}");
    assert!(
        devices.contains(&"usb-storage,drive=m0,removable=on,bus=xhci.0"),
        "{devices:?}"
    );
    assert!(
        devices.contains(&"usb-storage,drive=m1,removable=on,bus=xhci.0"),
        "{devices:?}"
    );
    let controller = devices
        .iter()
        .position(|device| *device == "qemu-xhci,id=xhci")
        .unwrap();
    let first_disk = devices
        .iter()
        .position(|device| device.starts_with("usb-storage"))
        .unwrap();
    assert!(
        controller < first_disk,
        "the bus exists before a device sits on it: {devices:?}"
    );
    let drives = values_of(&args, "-drive");
    assert!(
        drives.contains(&"if=none,id=m0,file=inst/answer.img,format=raw"),
        "{drives:?}"
    );
    assert!(
        drives.contains(&"if=none,id=m1,file=inst/seed.img,format=raw,readonly=on"),
        "read-only only when asked: {drives:?}"
    );
}

#[skuld::test]
fn a_machine_without_usb_media_has_no_xhci_controller() {
    let args = args(&spec(), &paths());
    assert!(!args.iter().any(|arg| arg.contains("xhci")), "{args:?}");
}

#[skuld::test]
fn a_writable_answer_disk_is_attached_without_readonly() {
    let mut spec = spec();
    spec.media = vec![usb("inst/answer.img", None, false)];

    let args = args(&spec, &paths());

    let answer: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| arg.contains("answer.img"))
        .collect();
    assert_eq!(answer, ["if=none,id=m0,file=inst/answer.img,format=raw"]);
}

#[skuld::test]
fn a_usb_medium_takes_its_boot_index() {
    let mut spec = spec();
    spec.media = vec![usb("inst/boot.img", Some(1), true)];
    let args = args(&spec, &paths());
    assert!(
        values_of(&args, "-device").contains(&"usb-storage,drive=m0,removable=on,bus=xhci.0,bootindex=1"),
        "{args:?}"
    );
}

#[skuld::test]
fn the_vga_profile_adds_a_vga_device_and_keeps_display_none() {
    let mut spec = spec();
    spec.profile.video = Video::Vga;

    let args = args(&spec, &paths());

    assert!(values_of(&args, "-device").contains(&"VGA"), "{args:?}");
    assert_eq!(values_of(&args, "-display"), ["none"]);
    let without = self::args(&self::spec(), &paths());
    assert!(!values_of(&without, "-device").contains(&"VGA"), "{without:?}");
}

#[skuld::test]
fn an_arm64_guest_with_a_vga_adapter_is_refused_under_every_accelerator() {
    for accel in [Accel::Tcg, Accel::Kvm, Accel::Hvf] {
        let mut spec = spec();
        spec.arch = Arch::Arm64;
        spec.accel = accel;
        spec.profile.video = Video::Vga;

        let error = build_args(&spec, &paths()).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::NotSupported, "{accel:?}: {error}");
        assert!(error.message().contains("VGA"), "{accel:?}: {error}");
        spec.profile.video = Video::None;
        assert!(
            build_args(&spec, &paths()).is_ok(),
            "{accel:?}: the same machine without video"
        );
    }
}

/// The Linux guest's machine: the read-only virtio-blk seed and no video adapter.
#[skuld::test]
fn the_linux_profile_args_are_unchanged() {
    let mut spec = fake_spec(
        PathBuf::from("inst/disk.img"),
        PathBuf::from("inst/serial.log"),
        Lifetime::OutlivesOwner,
    );
    spec.accel = Accel::Whpx;
    spec.cpus = 2;
    spec.memory = 2048 << 20;
    spec.media = vec![Media {
        path: PathBuf::from("inst/cidata.img"),
        kind: MediaKind::Disk,
        boot_index: None,
        readonly: true,
        format: MediaFormat::Raw,
    }];

    let expected = [
        "-accel",
        "whpx",
        "-machine",
        "q35",
        "-cpu",
        "max",
        "-smp",
        "2",
        "-m",
        "2048",
        "-display",
        "none",
        "-nodefaults",
        "-no-user-config",
        "-rtc",
        "base=utc",
        "-chardev",
        "socket,id=qmp,path=tmp/q.sock,server=on,wait=off",
        "-object",
        "monitor-qmp,id=qmp0,chardev=qmp",
        "-chardev",
        "file,id=ser0,path=inst/serial.log",
        "-serial",
        "chardev:ser0",
        "-drive",
        "if=pflash,format=raw,readonly=on,file=share/edk2-x86_64-code.fd",
        "-drive",
        "if=pflash,format=raw,file=inst/vars.fd",
        "-drive",
        "if=none,id=d0,file=inst/disk.img,format=qcow2",
        "-device",
        "virtio-blk-pci,drive=d0,bootindex=1",
        "-drive",
        "if=none,id=m0,file=inst/cidata.img,format=raw,readonly=on",
        "-device",
        "virtio-blk-pci,drive=m0",
        "-netdev",
        "user,id=n0,hostfwd=tcp:127.0.0.1:0-:22",
        "-device",
        "virtio-net-pci,netdev=n0",
    ];
    assert_eq!(args(&spec, &paths()), expected);
}

/// The install media of a Windows build.
#[skuld::test]
fn an_install_cdrom_is_an_ide_cd_on_an_ahci_port() {
    let mut spec = spec();
    spec.media = vec![Media {
        path: PathBuf::from("inst/overlay.qcow2"),
        kind: MediaKind::Cdrom,
        boot_index: Some(1),
        readonly: true,
        format: MediaFormat::Raw,
    }];
    let args = args(&spec, &paths());
    assert!(
        values_of(&args, "-device").contains(&"ide-cd,drive=m0,bus=ide.0,bootindex=1"),
        "{args:?}"
    );
}

fn cdroms(count: usize) -> Vec<Media> {
    (0..count)
        .map(|i| Media {
            path: PathBuf::from(format!("cd{i}.iso")),
            kind: MediaKind::Cdrom,
            boot_index: None,
            readonly: true,
            format: MediaFormat::Raw,
        })
        .collect()
}

/// q35's built-in AHCI controller is the bus `ide`, with one unit on each of its ports.
#[skuld::test]
fn each_cdrom_gets_its_own_ahci_port_in_order() {
    let mut spec = spec();
    spec.media = cdroms(2);
    spec.media.insert(
        1,
        Media {
            path: PathBuf::from("data.img"),
            kind: MediaKind::Disk,
            boot_index: None,
            readonly: false,
            format: MediaFormat::Raw,
        },
    );
    spec.media.push(Media {
        path: PathBuf::from("boot.iso"),
        kind: MediaKind::Cdrom,
        boot_index: Some(1),
        readonly: true,
        format: MediaFormat::Raw,
    });

    let args = args(&spec, &paths());

    // Boot order first, so `boot.iso` is m0 and takes port 0; the other CD-ROMs follow in their order.
    let devices: Vec<&str> = values_of(&args, "-device")
        .into_iter()
        .filter(|device| device.starts_with("ide-cd"))
        .collect();
    assert_eq!(
        devices,
        [
            "ide-cd,drive=m0,bus=ide.0,bootindex=1",
            "ide-cd,drive=m1,bus=ide.1",
            "ide-cd,drive=m3,bus=ide.2",
        ]
    );
}

#[skuld::test]
fn as_many_cdroms_as_ahci_ports_fit_and_one_more_is_refused() {
    let mut spec = spec();
    spec.media = cdroms(AHCI_PORTS);
    let args = args(&spec, &paths());
    let buses: Vec<String> = values_of(&args, "-device")
        .into_iter()
        .filter_map(|device| device.split(',').find_map(|part| part.strip_prefix("bus=ide.")))
        .map(str::to_owned)
        .collect();
    assert_eq!(buses, (0..AHCI_PORTS).map(|port| port.to_string()).collect::<Vec<_>>());

    spec.media = cdroms(AHCI_PORTS + 1);
    let error = build_args(&spec, &paths()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Usage, "{error}");
    assert!(
        error.message().contains(&format!("at most {AHCI_PORTS} CD-ROMs")),
        "{error}"
    );
    assert!(error.message().contains(&format!("cd{AHCI_PORTS}.iso")), "{error}");
}
