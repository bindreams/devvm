//! Guests for the tests: they need no image, no network and no SSH.

use std::path::Path;
use std::sync::Arc;

use crate::access::SshSession;
use crate::backend::{DeviceProfile, DiskBus, Media, MediaKind, NetModel};
use crate::guest::linux::LinuxGuest;
use crate::guest::{BaseBuilder, GuestOs, Guests, Readiness, SeedMedia, SeedSpec, posix_root, windows_root};
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
        LinuxGuest.boot_id_command()
    }

    fn wrap(&self, cmd: &Command) -> Result<String> {
        if cmd.argv.is_empty() {
            return Err(Error::new(ErrorKind::Usage, "a command needs a non-empty argv"));
        }
        Ok(cmd.argv.join(" "))
    }

    fn sftp_path(&self, guest_path: &str, _home: &str) -> String {
        match &self.paths {
            SftpPaths::WithoutDrive(drive) => match guest_path.strip_prefix(drive) {
                // The drive's root is the server's.
                Some("") => "/".to_owned(),
                Some(inside) => inside.to_owned(),
                None => guest_path.to_owned(),
            },
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

    fn split_root<'a>(&self, path: &'a str) -> Result<(String, &'a str)> {
        if let SftpPaths::WithoutDrive(drive) = &self.paths
            && let Some(rest) = path.strip_prefix(drive)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            return Ok((format!("{drive}/"), rest.trim_start_matches('/')));
        }
        match self.kind {
            GuestKind::Windows => windows_root(path),
            GuestKind::Linux | GuestKind::Macos => Ok(posix_root(path)),
        }
    }

    fn user_path(&self, sftp_path: &str) -> String {
        match &self.paths {
            SftpPaths::WithoutDrive(drive) if sftp_path.starts_with('/') => format!("{drive}{sftp_path}"),
            SftpPaths::Under(root) => match sftp_path.strip_prefix(root.trim_end_matches('/')) {
                Some("") => "/".to_owned(),
                Some(inside) if inside.starts_with('/') => inside.to_owned(),
                _ => sftp_path.to_owned(),
            },
            SftpPaths::Same | SftpPaths::WithoutDrive(_) => sftp_path.to_owned(),
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

    fn wait_ready(&self, _ssh: &SshSession) -> Result<Readiness> {
        Ok(Readiness::Ready)
    }

    /// As in a Linux guest.
    fn shell_argv(&self, shell: Option<Shell>, script: &str) -> Result<Vec<String>> {
        LinuxGuest.shell_argv(shell, script)
    }

    fn file_name_problem(&self, name: &str) -> Option<String> {
        match self.kind {
            GuestKind::Windows => names::windows_problem(name),
            GuestKind::Linux | GuestKind::Macos => names::posix_problem(name),
        }
    }
}
