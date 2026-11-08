//! Guests for the tests: they need no image, no network and no SSH.

use std::path::Path;
use std::sync::{Arc, Mutex};

use secrecy::ExposeSecret;

use crate::access::SshSession;
use crate::backend::{DeviceProfile, DiskBus, Media, MediaKind, NetModel};
use crate::exit::ExitStatus;
use crate::guest::linux::LinuxGuest;
use crate::guest::{BaseBuilder, GuestOs, Guests, Readiness, SeedKeys, SeedMedia, SeedSpec, posix_root, windows_root};
use crate::host::{Host, names};
use crate::instance::record::InstanceId;
use crate::recipe::Shell;
use crate::test_support::sshd::TestSshd;
use crate::{Command, Error, ErrorKind, GuestKind, Result};

pub(crate) struct FakeGuests;

impl FakeGuests {
    /// A guest that spells its paths with the drive `drive` (`Z:`), which its SFTP server leaves out.
    pub(crate) fn os_on_drive(&self, kind: GuestKind, drive: &'static str) -> Arc<dyn GuestOs> {
        Arc::new(FakeGuestOs {
            kind,
            paths: SftpPaths::WithoutDrive(drive),
            served: None,
        })
    }

    /// A guest whose `/` is the directory `root` of the machine its SFTP server runs on, as the server sees it.
    pub(crate) fn os_rooted_at(&self, kind: GuestKind, root: &Path) -> Arc<dyn GuestOs> {
        Arc::new(FakeGuestOs {
            kind,
            paths: SftpPaths::Under(root.to_string_lossy().into_owned()),
            served: None,
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
            served: None,
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
    /// Set for the guests of [`ServedGuests`]: what they do with a seed and with a boot.
    served: Option<Arc<Served>>,
}

impl GuestOs for FakeGuestOs {
    /// A file in `seed.dir` naming the host.
    fn instance_seed(&self, _host: &dyn Host, seed: &SeedSpec) -> Result<SeedMedia> {
        let path = seed.dir.join("seed.fake");
        std::fs::write(
            &path,
            format!("hostname={}\ninstance-id={}", seed.hostname, seed.instance_id),
        )
        .map_err(|e| Error::io(ErrorKind::Io, format!("cannot write {}", path.display()), e))?;
        // Only a seed that was written reaches the guest.
        if let Some(served) = &self.served {
            served.read_seed(seed);
        }
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

    fn wait_ready(&self, ssh: &SshSession) -> Result<Readiness> {
        match &self.served {
            Some(served) => served.wait_ready(ssh),
            None => Ok(Readiness::Ready),
        }
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

// Served guests =======================================================================================================

/// What a started guest got from its seed, as the fake guest saw it.
#[derive(Clone, Debug)]
pub(crate) struct SeedSeen {
    pub hostname: String,
    pub instance_id: InstanceId,
    pub keys: SeedKeys,
}

/// Guests whose machines are served by a [`TestSshd`]: reading a seed makes the server trust the seed's client key
/// and present its host key, as a booted guest's `sshd` does, and `wait_ready` runs a command over the session.
/// Give the backend the server's address (`FakeBackend::with_ssh_endpoint`) so that a started machine is reached there.
pub(crate) struct ServedGuests(Arc<Served>);

struct Served {
    server: Arc<TestSshd>,
    seeds: Mutex<Vec<SeedSeen>>,
    /// The command `wait_ready` runs; it is ready when it exits 0.
    ready_command: Mutex<String>,
    /// What `wait_ready` answers after the command succeeded.
    degraded: Mutex<Option<String>>,
    /// Called first in every `wait_ready`.
    on_ready: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl ServedGuests {
    pub(crate) fn new(server: Arc<TestSshd>) -> ServedGuests {
        ServedGuests(Arc::new(Served {
            server,
            seeds: Mutex::default(),
            ready_command: Mutex::new("echo ready".to_owned()),
            degraded: Mutex::default(),
            on_ready: Mutex::default(),
        }))
    }

    /// The command that `wait_ready` runs through the session (`echo ready`): it is ready when the command exits 0.
    pub(crate) fn ready_when(&self, command: &str) {
        *self.0.ready_command.lock().unwrap() = command.to_owned();
    }

    /// `wait_ready` finds the guest [`Readiness::Degraded`], for the reason `what`.
    pub(crate) fn degraded(&self, what: &str) {
        *self.0.degraded.lock().unwrap() = Some(what.to_owned());
    }

    /// Runs `hook` at the start of every `wait_ready`, before it talks to the guest.
    pub(crate) fn on_ready(&self, hook: impl Fn() + Send + 'static) {
        *self.0.on_ready.lock().unwrap() = Some(Box::new(hook));
    }

    /// Every seed the guests were asked for, in order.
    pub(crate) fn seeds(&self) -> Vec<SeedSeen> {
        self.0.seeds.lock().unwrap().clone()
    }
}

impl Served {
    fn read_seed(&self, seed: &SeedSpec) {
        self.server
            .install(&seed.keys.client_public, seed.keys.host_private.expose_secret());
        self.seeds.lock().unwrap().push(SeedSeen {
            hostname: seed.hostname.clone(),
            instance_id: seed.instance_id,
            keys: seed.keys.clone(),
        });
    }

    fn wait_ready(&self, ssh: &SshSession) -> Result<Readiness> {
        if let Some(hook) = self.on_ready.lock().unwrap().as_ref() {
            hook();
        }
        let command = self.ready_command.lock().unwrap().clone();
        let output = ssh.exec_collect(&command)?;
        if output.status != ExitStatus::Code(0) {
            return Err(Error::new(
                ErrorKind::Internal,
                format!(
                    "the fake guest's readiness command `{command}` ended with {:?}",
                    output.status
                ),
            ));
        }
        Ok(match self.degraded.lock().unwrap().clone() {
            Some(what) => Readiness::Degraded(what),
            None => Readiness::Ready,
        })
    }
}

impl Guests for ServedGuests {
    fn os(&self, kind: GuestKind) -> Result<Arc<dyn GuestOs>> {
        Ok(Arc::new(FakeGuestOs {
            kind,
            paths: SftpPaths::Same,
            served: Some(self.0.clone()),
        }))
    }

    fn builder(&self, kind: GuestKind) -> Result<Arc<dyn BaseBuilder>> {
        FakeGuests.builder(kind)
    }
}
