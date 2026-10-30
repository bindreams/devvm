//! The Linux guest: how commands are wrapped for the guest shell, the first-boot seed, and the root layer build.

use crate::backend::{DeviceProfile, DiskBus, NetModel};
use crate::guest::{SeedMedia, SeedSpec};
use crate::media::write_cidata_image;
use crate::recipe::Shell;
use crate::{Command, Error, ErrorKind, Result};

mod ready;
mod seed;
mod wrap;

#[cfg(test)]
#[path = "linux_tests.rs"]
mod linux_tests;

/// The cloud image's guests: Ubuntu 24.04 with cloud-init, reached as `user` over SSH.
pub(crate) struct LinuxGuest;

/// Written next to the instance's other scratch files; its name is not part of any contract.
const SEED_IMAGE: &str = "cidata.img";

impl LinuxGuest {
    pub(crate) fn instance_seed(&self, spec: &SeedSpec) -> Result<SeedMedia> {
        let media = write_cidata_image(&seed::seed_files(spec)?, &spec.dir.join(SEED_IMAGE))?;
        Ok(SeedMedia { media })
    }

    pub(crate) fn boot_id_command(&self) -> Command {
        Command::new(["cat", "/proc/sys/kernel/random/boot_id"])
    }

    /// The string sshd hands to the account's shell for `cmd`; `usage` for an empty argv, a bad environment name or a
    /// NUL byte.
    pub(crate) fn wrap(&self, cmd: &Command) -> Result<String> {
        wrap::wrap(cmd)
    }

    /// Bumped whenever `wrap`, `user:`, `cwd:` or elevation behavior changes.
    pub(crate) fn semantics_version(&self) -> u32 {
        1
    }

    pub(crate) fn device_profile(&self) -> DeviceProfile {
        DeviceProfile {
            disk: DiskBus::Virtio,
            net: NetModel::Virtio,
            no_reboot: false,
        }
    }

    pub(crate) fn shell_argv(&self, shell: Option<Shell>, script: &str) -> Result<Vec<String>> {
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
}
