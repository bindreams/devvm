//! QEMU's command line, built from a `MachineSpec` and the paths the backend resolved. Pure: nothing here touches the
//! file system or starts anything. Every argument is its own argv item (no shell); a comma in a path is doubled in
//! option values, as QEMU's option parser requires.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::directory_of;
use crate::backend::{DiskBus, Endpoint, MachineSpec, Media, MediaKind, NetModel};
use crate::host::Accel;
use crate::{Arch, Error, ErrorKind, Result};

#[cfg(test)]
#[path = "args_tests.rs"]
mod args_tests;

/// What the backend resolves for one launch: the QMP socket, and the firmware (none for a machine without UEFI).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LaunchPaths {
    pub qmp: PathBuf,
    /// The read-only firmware code image.
    pub code: Option<PathBuf>,
    /// This machine's writable firmware variables.
    pub vars: Option<PathBuf>,
}

impl LaunchPaths {
    /// The paths for `spec`: the firmware code the spec names, else `<share>/edk2-<arch>-code.fd`; the variables the
    /// spec names, else `vars.fd` beside the machine's disk. `Internal` when the disk has no directory.
    pub(crate) fn resolve(spec: &MachineSpec, qmp: PathBuf, share: &Path) -> Result<LaunchPaths> {
        let named = spec.firmware.as_ref();
        let code = named
            .map(|firmware| firmware.code.clone())
            .filter(|code| !code.as_os_str().is_empty())
            .unwrap_or_else(|| share.join(format!("edk2-{}-code.fd", qemu_arch(spec.arch))));
        let vars = match named.and_then(|firmware| firmware.vars.clone()) {
            Some(vars) => vars,
            None => directory_of(&spec.disk, "the machine's disk")?.join("vars.fd"),
        };
        Ok(LaunchPaths {
            qmp,
            code: Some(code),
            vars: Some(vars),
        })
    }
}

/// How QEMU names an architecture, in `qemu-system-<arch>` and in its firmware files.
pub(super) fn qemu_arch(arch: Arch) -> &'static str {
    match arch {
        Arch::Amd64 => "x86_64",
        Arch::Arm64 => "aarch64",
    }
}

/// The guest port a forward reaches. `Elevated` has none until the Windows guest exists.
pub(crate) fn guest_port(endpoint: Endpoint) -> Result<u16> {
    match endpoint {
        Endpoint::Normal => Ok(22),
        Endpoint::Elevated => Err(Error::new(
            ErrorKind::NotSupported,
            "the elevated endpoint has no guest port yet: it belongs to the Windows guest",
        )),
    }
}

/// `path` as an option value: valid Unicode, with every comma doubled.
pub(crate) fn escape_option_value(path: &Path) -> Result<String> {
    let text = path.to_str().ok_or_else(|| {
        Error::new(
            ErrorKind::Unavailable,
            format!(
                "the path {} is not valid Unicode, so QEMU cannot be given it",
                path.display()
            ),
        )
    })?;
    Ok(text.replace(',', ",,"))
}

fn accel_name(accel: Accel) -> &'static str {
    match accel {
        Accel::Whpx => "whpx",
        Accel::Kvm => "kvm",
        Accel::Hvf => "hvf",
        Accel::Tcg => "tcg",
    }
}

/// The argument list for `spec` (without the executable).
pub(crate) fn build_args(spec: &MachineSpec, paths: &LaunchPaths) -> Result<Vec<OsString>> {
    let mut args = Args(Vec::new());
    let machine = match spec.arch {
        Arch::Amd64 => "q35",
        Arch::Arm64 if spec.accel == Accel::Whpx => {
            return Err(Error::new(
                ErrorKind::NotSupported,
                "WHPX cannot run an arm64 guest; arm64 runs under software emulation only",
            ));
        }
        Arch::Arm64 => "virt,gic-version=3",
    };
    args.pair("-accel", accel_name(spec.accel));
    args.pair("-machine", machine);
    args.pair("-cpu", "max");
    args.pair("-smp", spec.cpus.to_string());
    args.pair("-m", (spec.memory >> 20).to_string());
    args.pair("-display", "none");
    args.one("-nodefaults");
    args.one("-no-user-config");
    args.pair("-rtc", "base=utc");
    if spec.profile.no_reboot {
        args.one("-no-reboot");
    }

    // Control socket and serial console.
    args.pair(
        "-chardev",
        format!(
            "socket,id=qmp,path={},server=on,wait=off",
            escape_option_value(&paths.qmp)?
        ),
    );
    args.pair("-object", "monitor-qmp,id=qmp0,chardev=qmp");
    args.pair(
        "-chardev",
        format!("file,id=ser0,path={}", escape_option_value(&spec.serial_log)?),
    );
    args.pair("-serial", "chardev:ser0");

    // Firmware.
    if let Some(code) = &paths.code {
        args.pair(
            "-drive",
            format!("if=pflash,format=raw,readonly=on,file={}", escape_option_value(code)?),
        );
    }
    if let Some(vars) = &paths.vars {
        args.pair(
            "-drive",
            format!("if=pflash,format=raw,file={}", escape_option_value(vars)?),
        );
    }

    // Media that boot come first, in boot order; the system disk boots after all of them.
    let mut media: Vec<&Media> = spec.media.iter().collect();
    media.sort_by_key(|medium| medium.boot_index.unwrap_or(u32::MAX));
    let disk_boot_index = disk_boot_index(&spec.media)?;
    args.pair(
        "-drive",
        format!("if=none,id=d0,file={},format=qcow2", escape_option_value(&spec.disk)?),
    );
    let system_disk = system_disk_device(spec.profile.disk, disk_boot_index, &mut args);
    args.pair("-device", system_disk);
    for (index, medium) in media.iter().enumerate() {
        medium_args(&mut args, index, medium)?;
    }

    // User-mode network with loopback forwards on ephemeral ports.
    let mut netdev = String::from("user,id=n0");
    for endpoint in &spec.forwards {
        netdev.push_str(&format!(",hostfwd=tcp:127.0.0.1:0-:{}", guest_port(*endpoint)?));
    }
    if spec.network == crate::Network::None {
        netdev.push_str(",restrict=on");
    }
    args.pair("-netdev", netdev);
    args.pair(
        "-device",
        match spec.profile.net {
            NetModel::Virtio => "virtio-net-pci,netdev=n0",
            NetModel::E1000 => "e1000,netdev=n0",
        },
    );
    Ok(args.0)
}

/// The boot index of the system disk: one after the last medium that boots. QEMU's `bootindex` is a signed 32-bit
/// number, so a medium whose index leaves no room after it is refused, naming the medium.
fn disk_boot_index(media: &[Media]) -> Result<u32> {
    const LAST: u32 = i32::MAX as u32;
    let mut after = 1;
    for medium in media {
        let Some(index) = medium.boot_index else { continue };
        if index >= LAST {
            return Err(Error::new(
                ErrorKind::Usage,
                format!(
                    "the boot index {index} of {} is too large: QEMU's limit is {}, and the system disk boots after every medium",
                    medium.path.display(),
                    LAST - 1
                ),
            ));
        }
        after = after.max(index + 1);
    }
    Ok(after)
}

/// The `-device` value of the system disk. A SATA disk also needs its controller, added to `args` first.
fn system_disk_device(bus: DiskBus, boot_index: u32, args: &mut Args) -> String {
    match bus {
        DiskBus::Virtio => format!("virtio-blk-pci,drive=d0,bootindex={boot_index}"),
        DiskBus::Sata => {
            args.pair("-device", "ich9-ahci,id=ahci");
            format!("ide-hd,drive=d0,bus=ahci.0,bootindex={boot_index}")
        }
        DiskBus::Nvme => format!("nvme,drive=d0,serial=devvm,bootindex={boot_index}"),
    }
}

fn medium_args(args: &mut Args, index: usize, medium: &Media) -> Result<()> {
    let file = escape_option_value(&medium.path)?;
    let readonly = if medium.readonly { ",readonly=on" } else { "" };
    let boot = medium
        .boot_index
        .map_or_else(String::new, |boot_index| format!(",bootindex={boot_index}"));
    match medium.kind {
        MediaKind::Cdrom => {
            args.pair(
                "-drive",
                format!("if=none,id=m{index},file={file},format=raw,media=cdrom{readonly}"),
            );
            args.pair("-device", format!("ide-cd,drive=m{index}{boot}"));
        }
        MediaKind::Disk => {
            args.pair(
                "-drive",
                format!("if=none,id=m{index},file={file},format=raw{readonly}"),
            );
            args.pair("-device", format!("virtio-blk-pci,drive=m{index}{boot}"));
        }
    }
    Ok(())
}

struct Args(Vec<OsString>);

impl Args {
    fn one(&mut self, flag: &str) {
        self.0.push(flag.into());
    }

    fn pair(&mut self, flag: &str, value: impl Into<OsString>) {
        self.0.push(flag.into());
        self.0.push(value.into());
    }
}
