//! Guest operating systems: what differs between Linux, Windows and macOS guests, behind traits.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use secrecy::{ExposeSecret, SecretString};

use crate::access::SshSession;
use crate::backend::{DeviceProfile, Media};
use crate::exit::Progress;
use crate::host::Host;
use crate::instance::record::InstanceId;
use crate::iso::IsoRoot;
use crate::recipe::{Resources, Setting, Shell, Source};
use crate::store::layers::{Clock, LayerDir, LayerMeta};
use crate::{Arch, Command, Dirs, Error, ErrorKind, GuestKind, Result};

#[cfg(test)]
#[path = "guest_tests.rs"]
mod guest_tests;

#[cfg(test)]
pub(crate) mod fake;
pub(crate) mod linux;
pub(crate) mod windows;

/// Written next to the instance's other scratch files; its name is not part of any contract.
pub(crate) const SEED_IMAGE: &str = "cidata.img";

pub(crate) trait GuestOs: Send + Sync {
    /// The per-instance first-boot media (account, keys, hostname). It holds the guest's private key, so `host` writes
    /// it as a private file.
    fn instance_seed(&self, host: &dyn Host, seed: &SeedSpec) -> Result<SeedMedia>;
    /// A command whose output is the guest's boot id.
    fn boot_id_command(&self) -> Command;
    /// A command that returns once every write the guest has accepted is on its disk.
    fn flush_command(&self) -> Command;
    /// The final string the guest's shell executes for `cmd`. A command no shell string can carry is a `usage` error
    /// naming the cause: an empty argv or working directory, a NUL byte, or a string over the guest's single-argument
    /// size limit.
    fn wrap(&self, cmd: &Command) -> Result<String>;
    /// `guest_path` as the guest's SFTP server expects it.
    fn sftp_path(&self, guest_path: &str, home: &str) -> String;
    /// The guest path that `sftp_path` turned into `sftp_path`, for a message that names the path as the user wrote it.
    fn user_path(&self, sftp_path: &str) -> String;
    /// Splits the guest path `path`, as the user writes it, into its root and the rest. The root is spelled the way
    /// the guest does, with a trailing `/`: `/`, `C:/`, `//server/share/`; it is empty for a relative path. `..` never
    /// goes above it. The default is a POSIX guest's: one `/`. A path the guest does not place by name is a `usage`
    /// error naming it.
    fn split_root<'a>(&self, path: &'a str) -> Result<(String, &'a str)> {
        Ok(posix_root(path))
    }
    /// Bumped whenever `wrap`, `user:`, `cwd:` or elevation behavior changes; part of every layer key.
    fn semantics_version(&self) -> u32;
    fn device_profile(&self) -> DeviceProfile;
    /// Blocks until the guest's boot has finished. An unfinished or failed boot is an error; a boot that finished
    /// with recoverable problems is [`Readiness::Degraded`], for the caller to report. A `connection-lost` error is the
    /// caller's to retry on a new connection while the VM lives and the boot bound has not passed: a guest may restart
    /// during its boot.
    fn wait_ready(&self, ssh: &SshSession) -> Result<Readiness>;
    /// The command line that runs `script` in `shell` (`None`: the guest's default), for a recipe `run` step. A shell
    /// the guest does not have is a `recipe` error.
    fn shell_argv(&self, shell: Option<Shell>, script: &str) -> Result<Vec<String>>;
    /// The command that applies `setting`, run as a recipe step does (elevated if it says so). A guest that does not
    /// have the setting says so with a `recipe` error naming it and the guest.
    fn setting_command(&self, setting: &Setting) -> Result<Command>;
    /// Why `name`, a file name taken from the host, cannot be one file name in a guest directory (a Windows guest
    /// refuses `a?b` and device names); `None` when it can.
    fn file_name_problem(&self, name: &str) -> Option<String>;
    /// The command of a `copy` step that makes `destination`, extracts the tar at `archive` into it (over files that are
    /// there) and removes `archive` whatever the extraction did, ending with the extraction's status. Both are guest
    /// paths as the user types them, absolute; the command quotes them. It runs as the user.
    fn extract_command(&self, destination: &str, archive: &str) -> Result<Command>;
    /// The directory a `copy` step uploads its archive to, in the form a guest path is typed: one the user can write and
    /// whose files are not part of any layer once the step has removed them.
    fn copy_scratch_dir(&self) -> &'static str;
}

/// [`GuestOs::split_root`] for a path whose only root is `/`; any number of leading `/` are one.
pub(crate) fn posix_root(path: &str) -> (String, &str) {
    match path.strip_prefix('/') {
        Some(_) => ("/".to_owned(), path.trim_start_matches('/')),
        None => (String::new(), path),
    }
}

/// [`GuestOs::split_root`] for a Windows guest: `C:` (with or without `/` after it) and `//server/share` are roots,
/// otherwise as [`posix_root`]. `C:a` is relative to the current directory of drive `C:`, which a path cannot name:
/// it is refused, as is a UNC path with a server and no share, which a guest reads as a path on its current drive.
pub(crate) fn windows_root(path: &str) -> Result<(String, &str)> {
    let mut letters = path.chars();
    if let (Some(letter), Some(':')) = (letters.next(), letters.next())
        && letter.is_ascii_alphabetic()
    {
        let rest = &path[2..];
        if rest.is_empty() || rest.starts_with('/') {
            return Ok((format!("{letter}:/"), rest.trim_start_matches('/')));
        }
        if !rest.starts_with('\\') {
            return Err(Error::new(
                ErrorKind::Usage,
                format!(
                    "the guest path {path} is relative to the current directory of drive {letter}:; write {letter}:/{rest} to start at the root of drive {letter}:"
                ),
            ));
        }
    }
    if let Some(unc) = path.strip_prefix("//") {
        let (server, after_server) = unc.split_once('/').unwrap_or((unc, ""));
        if !server.is_empty() {
            let after_server = after_server.trim_start_matches('/');
            let (share, rest) = after_server.split_once('/').unwrap_or((after_server, ""));
            if share.is_empty() {
                return Err(Error::new(
                    ErrorKind::Usage,
                    format!("the guest path {path} has a server and no share; a UNC path needs //{server}/share"),
                ));
            }
            return Ok((format!("//{server}/{share}/"), rest.trim_start_matches('/')));
        }
    }
    Ok(posix_root(path))
}

/// How a finished boot went.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Readiness {
    Ready,
    /// The guest is usable, but its boot finished with recoverable problems; the text names them.
    Degraded(String),
}

pub(crate) trait BaseBuilder: Send + Sync {
    /// Builds the root layer from `src` into `staging`; the returned record contributes its expiry.
    fn build_base(&self, src: &Source, ctx: &BuildCtx, staging: &LayerDir) -> Result<LayerMeta>;
    /// Hash of the build definition (templates, configs, pins); part of every root layer key.
    fn definition_hash(&self) -> [u8; 32];
}

/// The guest implementations a build knows.
pub(crate) trait Guests: Send + Sync {
    fn os(&self, kind: GuestKind) -> Result<Arc<dyn GuestOs>>;
    fn builder(&self, kind: GuestKind) -> Result<Arc<dyn BaseBuilder>>;
}

pub(crate) struct SeedSpec {
    pub hostname: String,
    /// A scratch directory the seed may be written into.
    pub dir: PathBuf,
    /// The instance the seed is for; it stays the same across restarts.
    pub instance_id: InstanceId,
    pub keys: SeedKeys,
}

/// The SSH keys a first boot installs, as the text of their files. The private key is a secret: it stays out of `Debug`
/// output.
#[derive(Clone, Debug)]
pub(crate) struct SeedKeys {
    /// The public key of the client allowed to log in.
    pub client_public: String,
    /// The guest's SSH host key pair.
    pub host_private: SecretString,
    pub host_public: String,
}

impl SeedKeys {
    /// An error (kind `internal`) naming the first key that is empty: a seed without it would boot a guest nobody can
    /// log in to, or one with no host key.
    pub(crate) fn require_present(&self) -> Result<()> {
        for (name, empty) in [
            ("client_public", self.client_public.is_empty()),
            ("host_private", self.host_private.expose_secret().is_empty()),
            ("host_public", self.host_public.is_empty()),
        ] {
            if empty {
                return Err(Error::new(
                    ErrorKind::Internal,
                    format!("the seed key `{name}` is empty, so the guest would boot without it"),
                ));
            }
        }
        Ok(())
    }
}

pub(crate) struct SeedMedia {
    pub media: Media,
}

/// What a base build knows of the engine that runs it.
pub(crate) struct BuildCtx {
    pub dirs: Dirs,
    pub arch: Arch,
    pub progress: Arc<dyn Progress>,
    /// The time a license read is counted from.
    pub clock: Arc<dyn Clock>,
    /// The recipe's resources: a base build is as large as the instances of the recipe.
    pub resources: Resources,
    /// The build's own directory, which is removed when the build ends: what a base build needs only while it runs goes
    /// here, not in its layer.
    pub work: PathBuf,
    /// The held ISO of an ISO root, which the build takes: it is read once, by the builder that installs from it, which
    /// keeps it held until the guest's disk has opened it. `None` for any other root, and once taken.
    pub iso: Mutex<Option<IsoRoot>>,
}

pub(crate) fn missing(kind: GuestKind) -> Error {
    Error::new(
        ErrorKind::NotSupported,
        format!("{} guests are not available in this build of devvm", kind.as_str()),
    )
}
