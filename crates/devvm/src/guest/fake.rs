//! Guests for the tests: they need no image, no network and no SSH.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use secrecy::ExposeSecret;

use crate::access::SshSession;
use crate::backend::fake::LAYER_DISK;
use crate::backend::{DeviceProfile, DiskBus, Media, MediaKind, NetModel};
use crate::exit::ExitStatus;
use crate::guest::linux::LinuxGuest;
use crate::guest::{
    BaseBuilder, BuildCtx, GuestOs, Guests, Readiness, SeedKeys, SeedMedia, SeedSpec, posix_root, windows_root,
};
use crate::host::{Host, names};
use crate::instance::record::InstanceId;
use crate::recipe::hash::LayerKey;
use crate::recipe::{Shell, Source};
use crate::store::layers::{Generation, LayerDir, LayerMeta};
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

    fn builder(&self, _kind: GuestKind) -> Result<Arc<dyn BaseBuilder>> {
        Ok(Arc::new(FakeBaseBuilder))
    }
}

/// Builds a root layer without an image: one file in `staging`, which the fake backend's machines sit on. Its definition
/// hash is a constant.
pub(crate) struct FakeBaseBuilder;

impl BaseBuilder for FakeBaseBuilder {
    fn build_base(&self, _src: &Source, _ctx: &BuildCtx, staging: &LayerDir) -> Result<LayerMeta> {
        let disk = staging.path().join(LAYER_DISK);
        std::fs::write(&disk, b"base disk")
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot write {}", disk.display()), e))?;
        Ok(LayerMeta {
            key: LayerKey::from_bytes([0; 16]),
            generation: Generation::from_bytes([0; 16]),
            parent: None,
            created: SystemTime::UNIX_EPOCH,
            expires: None,
        })
    }

    fn definition_hash(&self) -> [u8; 32] {
        [7; 32]
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

    /// A Linux guest's, except that a served guest's boot id is its boot number (see [`ServedGuests::boot_follows`]).
    fn boot_id_command(&self) -> Command {
        match &self.served {
            Some(served) => served.boot_id_command(),
            None => LinuxGuest.boot_id_command(),
        }
    }

    /// A Linux guest's, except that a served guest whose flush runs another command (`ServedGuests::flush_runs`).
    fn flush_command(&self) -> Command {
        match &self.served {
            Some(served) => served.flush_command(),
            None => LinuxGuest.flush_command(),
        }
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
    ready_command: Mutex<String>,
    degraded: Mutex<Option<String>>,
    on_ready: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// The resets that start a new boot, if the guests follow any.
    resets: Mutex<Option<Arc<AtomicUsize>>>,
    /// How many boot id reads after a reset fail, before the stale ones.
    failing_boot_reads: Mutex<usize>,
    /// How many boot ids after a reset still say the old boot.
    stale_boot_reads: Mutex<usize>,
    /// Every read of the boot id after a reset ends without an exit status.
    unreadable_boot_id: Mutex<bool>,
    /// Every read of the boot id prints nothing.
    empty_boot_id: Mutex<bool>,
    /// Every flush exits 1.
    flush_runs: Mutex<Option<String>>,
    boots: Mutex<Boots>,
}

/// Which reset the boot id was last read after, and how many of its reads may still fail or answer the old boot.
#[derive(Default)]
struct Boots {
    seen: usize,
    failing_left: usize,
    stale_left: usize,
}

impl ServedGuests {
    pub(crate) fn new(server: Arc<TestSshd>) -> ServedGuests {
        ServedGuests(Arc::new(Served {
            server,
            seeds: Mutex::default(),
            ready_command: Mutex::new("echo ready".to_owned()),
            degraded: Mutex::default(),
            on_ready: Mutex::default(),
            resets: Mutex::default(),
            failing_boot_reads: Mutex::default(),
            stale_boot_reads: Mutex::default(),
            unreadable_boot_id: Mutex::default(),
            empty_boot_id: Mutex::default(),
            flush_runs: Mutex::default(),
            boots: Mutex::default(),
        }))
    }

    /// The guests' boot number is the value of `resets`: a guest whose machine is reset boots again. Without this the
    /// boot number is always 0.
    pub(crate) fn boot_follows(&self, resets: Arc<AtomicUsize>) {
        *self.0.resets.lock().unwrap() = Some(resets);
    }

    /// After each reset, the first `reads` reads of the boot id fail, as they do while the guest goes down or its `sshd`
    /// restarts; the stale reads come after them.
    pub(crate) fn failing_boot_reads(&self, reads: usize) {
        *self.0.failing_boot_reads.lock().unwrap() = reads;
    }

    /// After a reset, every read of the boot id ends without an exit status: an error that is neither a lost connection
    /// nor a failed command.
    pub(crate) fn unreadable_boot_id(&self) {
        *self.0.unreadable_boot_id.lock().unwrap() = true;
    }

    /// Every read of the boot id exits 0 and prints nothing.
    pub(crate) fn empty_boot_id(&self) {
        *self.0.empty_boot_id.lock().unwrap() = true;
    }

    /// Every flush of a guest's disk runs `command` (a script of the test SSH server) instead of the guest's own.
    pub(crate) fn flush_runs(&self, command: &str) {
        *self.0.flush_runs.lock().unwrap() = Some(command.to_owned());
    }

    /// After each reset, the first `reads` reads of the boot id still answer the old boot, as a guest does until it has
    /// gone down.
    pub(crate) fn stale_boot_reads(&self, reads: usize) {
        *self.0.stale_boot_reads.lock().unwrap() = reads;
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
    /// A Linux guest's flush, or the command `flush_runs` set.
    fn flush_command(&self) -> Command {
        match &*self.flush_runs.lock().unwrap() {
            Some(command) => Command::new(command.split_whitespace()),
            None => LinuxGuest.flush_command(),
        }
    }

    /// What a read of the boot id runs: `exit 1` while the failing reads last, else the boot the guest is in now (the
    /// number of resets so far, or the one before while the stale reads last).
    fn boot_id_command(&self) -> Command {
        if *self.empty_boot_id.lock().unwrap() {
            return Command::new(["echo"]);
        }
        let Some(resets) = self.resets.lock().unwrap().clone() else {
            return Command::new(["echo", "boot-0"]);
        };
        let now = resets.load(Ordering::SeqCst);
        let mut boots = self.boots.lock().unwrap();
        if now != boots.seen {
            boots.seen = now;
            boots.failing_left = *self.failing_boot_reads.lock().unwrap();
            boots.stale_left = *self.stale_boot_reads.lock().unwrap();
        }
        if now > 0 && *self.unreadable_boot_id.lock().unwrap() {
            return Command::new(["close-no-status"]);
        }
        if boots.failing_left > 0 {
            boots.failing_left -= 1;
            return Command::new(["exit", "1"]);
        }
        let boot = if boots.stale_left > 0 {
            boots.stale_left -= 1;
            // A stale read follows a reset, so `now` is at least 1.
            now - 1
        } else {
            now
        };
        Command::new(["echo".to_owned(), format!("boot-{boot}")])
    }

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
