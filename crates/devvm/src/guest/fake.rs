//! Guests for the tests: they need no image, no network and no SSH.

use std::path::Path;
use std::sync::Arc;

use crate::access::SshSession;
use crate::backend::{DeviceProfile, DiskBus, Media, MediaKind, NetModel};
use crate::guest::{BaseBuilder, GuestOs, Guests, SeedMedia, SeedSpec};
use crate::host::names;
use crate::recipe::Shell;
use crate::{Command, Error, ErrorKind, GuestKind, Result};

pub(crate) struct FakeGuests;

impl FakeGuests {
    /// A guest that spells its paths with the drive `drive` (`Z:`), which its SFTP server leaves out.
    pub(crate) fn os_on_drive(&self, kind: GuestKind, drive: &'static str) -> Arc<dyn GuestOs> {
        Arc::new(FakeGuestOs {
            kind,
            paths: SftpPaths::WithoutDrive(drive),
        })
    }

    /// A guest whose `/` is the directory `root` of the machine its SFTP server runs on, as the server sees it.
    pub(crate) fn os_rooted_at(&self, kind: GuestKind, root: &Path) -> Arc<dyn GuestOs> {
        Arc::new(FakeGuestOs {
            kind,
            paths: SftpPaths::Under(root.to_string_lossy().into_owned()),
        })
    }
}

/// How a guest's paths become the paths its SFTP server expects.
enum SftpPaths {
    Same,
    /// The paths start with this drive, which the server's do not.
    WithoutDrive(&'static str),
    /// The server's paths start with this directory; one that does already is left as it is.
    Under(String),
}

impl Guests for FakeGuests {
    fn os(&self, kind: GuestKind) -> Result<Arc<dyn GuestOs>> {
        Ok(Arc::new(FakeGuestOs {
            kind,
            paths: SftpPaths::Same,
        }))
    }

    fn builder(&self, kind: GuestKind) -> Result<Arc<dyn BaseBuilder>> {
        Err(Error::new(
            ErrorKind::NotSupported,
            format!("the fake guests build no {} base", kind.as_str()),
        ))
    }
}

/// A guest whose file names follow the rules of its kind: a Windows guest refuses what Windows refuses.
struct FakeGuestOs {
    kind: GuestKind,
    paths: SftpPaths,
}

impl GuestOs for FakeGuestOs {
    /// A file in `seed.dir` naming the host.
    fn instance_seed(&self, seed: &SeedSpec) -> Result<SeedMedia> {
        let path = seed.dir.join("seed.fake");
        std::fs::write(
            &path,
            format!("hostname={}\ninstance-id={}", seed.hostname, seed.instance_id),
        )
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
        match &self.paths {
            SftpPaths::WithoutDrive(drive) => guest_path.strip_prefix(drive).unwrap_or(guest_path).to_owned(),
            SftpPaths::Under(root) if guest_path.starts_with('/') => {
                let inside = guest_path == root || guest_path.starts_with(&format!("{root}/"));
                if inside {
                    guest_path.to_owned()
                } else {
                    format!("{}{guest_path}", root.trim_end_matches('/'))
                }
            }
            SftpPaths::Same | SftpPaths::Under(_) => guest_path.to_owned(),
        }
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
                return Err(Error::new(
                    ErrorKind::Recipe,
                    "the shell `cmd` does not exist in this guest",
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
        match self.kind {
            GuestKind::Windows => names::windows_problem(name),
            GuestKind::Linux | GuestKind::Macos => names::posix_problem(name),
        }
    }

    fn name_key(&self, name: &str) -> String {
        match self.kind {
            GuestKind::Windows => names::windows_key(name),
            GuestKind::Linux | GuestKind::Macos => name.to_owned(),
        }
    }
}
