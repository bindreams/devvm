//! The Linux guest: how commands are wrapped for the guest shell, the first-boot seed, and the root layer build.

use std::sync::Arc;

use crate::access::SshSession;
use crate::access::paths::expand_guest_path;
use crate::backend::{DeviceProfile, DiskBus, NetModel};
use crate::guest::{BaseBuilder, GuestOs, Guests, Readiness, SeedMedia, SeedSpec, missing};
use crate::host::names;
use crate::media::write_cidata_image;
use crate::recipe::Shell;
use crate::{Command, Error, ErrorKind, GuestKind, Result};

pub(crate) mod builder;
mod ready;
mod seed;
mod wrap;

pub(crate) use builder::LinuxBuilder;

#[cfg(test)]
#[path = "linux_tests.rs"]
mod linux_tests;

/// The guests a build with the QEMU backend knows: Linux. Windows and macOS guests are not built yet.
pub(crate) struct LinuxGuests {
    builder: Arc<LinuxBuilder>,
}

impl LinuxGuests {
    pub(crate) fn new(builder: Arc<LinuxBuilder>) -> LinuxGuests {
        LinuxGuests { builder }
    }
}

impl Guests for LinuxGuests {
    fn os(&self, kind: GuestKind) -> Result<Arc<dyn GuestOs>> {
        match kind {
            GuestKind::Linux => Ok(Arc::new(LinuxGuest)),
            GuestKind::Windows | GuestKind::Macos => Err(missing(kind)),
        }
    }

    fn builder(&self, kind: GuestKind) -> Result<Arc<dyn BaseBuilder>> {
        match kind {
            GuestKind::Linux => Ok(self.builder.clone()),
            GuestKind::Windows | GuestKind::Macos => Err(missing(kind)),
        }
    }
}

/// The cloud image's guests: Ubuntu 24.04 with cloud-init, reached as `user` over SSH.
pub(crate) struct LinuxGuest;

/// Written next to the instance's other scratch files; its name is not part of any contract.
const SEED_IMAGE: &str = "cidata.img";

impl GuestOs for LinuxGuest {
    fn instance_seed(&self, spec: &SeedSpec) -> Result<SeedMedia> {
        let media = write_cidata_image(&seed::seed_files(spec)?, &spec.dir.join(SEED_IMAGE))?;
        Ok(SeedMedia { media })
    }

    fn boot_id_command(&self) -> Command {
        Command::new(["cat", "/proc/sys/kernel/random/boot_id"])
    }

    /// The string sshd hands to the account's shell for `cmd`; `usage` for an empty argv, a bad or readonly environment
    /// name or a NUL byte.
    fn wrap(&self, cmd: &Command) -> Result<String> {
        wrap::wrap(cmd)
    }

    /// Total. A `~user/...` path was refused where it entered (`check_guest_path`); here it passes unchanged.
    fn sftp_path(&self, guest_path: &str, home: &str) -> String {
        expand_guest_path(guest_path, home)
    }

    /// The server's paths are the guest's own, so there is nothing to undo; a `~` the user wrote is already expanded.
    fn user_path(&self, sftp_path: &str) -> String {
        sftp_path.to_owned()
    }

    // `split_root` is the trait's: Linux has one root, `/`, and the kernel reads any number of leading `/` as it.

    /// Bumped whenever `wrap`, `user:`, `cwd:` or elevation behavior changes.
    fn semantics_version(&self) -> u32 {
        1
    }

    fn device_profile(&self) -> DeviceProfile {
        DeviceProfile {
            disk: DiskBus::Virtio,
            net: NetModel::Virtio,
            no_reboot: false,
        }
    }

    /// Whatever else the command's output lost after its status arrived does not change what cloud-init reported.
    fn wait_ready(&self, ssh: &SshSession) -> Result<Readiness> {
        ready::readiness(&ssh.exec_collect(ready::CLOUD_INIT_STATUS)?)
    }

    fn shell_argv(&self, shell: Option<Shell>, script: &str) -> Result<Vec<String>> {
        let program: &[&str] = match shell {
            None | Some(Shell::Bash) => &["bash", "-c"],
            Some(Shell::Sh) => &["sh", "-c"],
            Some(Shell::Pwsh) => &["pwsh", "-NoProfile", "-Command"],
            Some(Shell::Cmd) => {
                return Err(Error::new(
                    ErrorKind::Recipe,
                    "the shell `cmd` does not exist in a Linux guest",
                ));
            }
        };
        Ok(program
            .iter()
            .map(|part| (*part).to_owned())
            .chain([script.to_owned()])
            .collect())
    }

    fn file_name_problem(&self, name: &str) -> Option<String> {
        names::posix_problem(name)
    }
}
