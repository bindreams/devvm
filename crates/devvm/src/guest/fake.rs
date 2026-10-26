//! Guests for the tests: they need no image, no network and no SSH.

use std::sync::Arc;

use crate::access::SshSession;
use crate::backend::{DeviceProfile, DiskBus, Media, MediaKind, NetModel};
use crate::guest::{BaseBuilder, GuestOs, Guests, SeedMedia, SeedSpec};
use crate::recipe::Shell;
use crate::{Command, Error, ErrorKind, GuestKind, Result};

pub(crate) struct FakeGuests;

impl Guests for FakeGuests {
    fn os(&self, _kind: GuestKind) -> Result<Arc<dyn GuestOs>> {
        Ok(Arc::new(FakeGuestOs))
    }

    fn builder(&self, kind: GuestKind) -> Result<Arc<dyn BaseBuilder>> {
        Err(Error::new(
            ErrorKind::NotSupported,
            format!("the fake guests build no {} base", kind.as_str()),
        ))
    }
}

struct FakeGuestOs;

impl GuestOs for FakeGuestOs {
    /// A file in `seed.dir` naming the host.
    fn instance_seed(&self, seed: &SeedSpec) -> Result<SeedMedia> {
        let path = seed.dir.join("seed.fake");
        std::fs::write(&path, format!("hostname={}\ninstance-id={}", seed.hostname, seed.instance_id))
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot write {}", path.display()), e))?;
        Ok(SeedMedia {
            media: Media {
                path,
                kind: MediaKind::Cdrom,
                boot_index: None,
                readonly: true,
            },
        })
    }

    fn boot_id_command(&self) -> Command {
        Command::new(["cat", "/proc/sys/kernel/random/boot_id"])
    }

    fn wrap(&self, cmd: &Command) -> String {
        cmd.argv.join(" ")
    }

    fn sftp_path(&self, guest_path: &str, _home: &str) -> String {
        guest_path.to_owned()
    }

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

    fn wait_ready(&self, _ssh: &SshSession) -> Result<()> {
        Ok(())
    }

    /// As on Linux: `bash` by default, `cmd` does not exist.
    fn shell_argv(&self, shell: Option<Shell>, script: &str) -> Result<Vec<String>> {
        let program: &[&str] = match shell {
            None | Some(Shell::Bash) => &["bash", "-c"],
            Some(Shell::Sh) => &["sh", "-c"],
            Some(Shell::Pwsh) => &["pwsh", "-NoProfile", "-Command"],
            Some(Shell::Cmd) => {
                return Err(Error::new(ErrorKind::Recipe, "the shell `cmd` does not exist in this guest"));
            }
        };
        Ok(program.iter().map(|part| (*part).to_owned()).chain([script.to_owned()]).collect())
    }
}
