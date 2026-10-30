//! Guest operating systems: what differs between Linux, Windows and macOS guests, behind traits.

use std::path::PathBuf;
use std::sync::Arc;

use crate::access::SshSession;
use crate::backend::{DeviceProfile, Media};
use crate::exit::Progress;
use crate::instance::record::InstanceId;
use crate::recipe::{Shell, Source};
use crate::store::layers::{LayerDir, LayerMeta};
use crate::{Arch, Command, Dirs, Error, ErrorKind, GuestKind, Result};

#[cfg(test)]
#[path = "guest_tests.rs"]
mod guest_tests;

#[cfg(test)]
pub(crate) mod fake;

pub(crate) trait GuestOs: Send + Sync {
    /// The per-instance first-boot media (account, keys, hostname).
    fn instance_seed(&self, seed: &SeedSpec) -> Result<SeedMedia>;
    /// A command whose output is the guest's boot id.
    fn boot_id_command(&self) -> Command;
    /// The final string the guest's shell executes for `cmd`.
    fn wrap(&self, cmd: &Command) -> String;
    /// `guest_path` as the guest's SFTP server expects it.
    fn sftp_path(&self, guest_path: &str, home: &str) -> String;
    /// Bumped whenever `wrap`, `user:`, `cwd:` or elevation behavior changes; part of every layer key.
    fn semantics_version(&self) -> u32;
    fn device_profile(&self) -> DeviceProfile;
    fn wait_ready(&self, ssh: &SshSession) -> Result<()>;
    /// The command line that runs `script` in `shell` (`None`: the guest's default), for a recipe `run` step. A shell
    /// the guest does not have is a `recipe` error.
    fn shell_argv(&self, shell: Option<Shell>, script: &str) -> Result<Vec<String>>;
    /// Why `name`, a file name taken from the host, cannot be one file name in a guest directory (a Windows guest
    /// refuses `a?b` and device names); `None` when it can.
    fn file_name_problem(&self, name: &str) -> Option<String>;
    /// `name` the way the guest's file system compares names: two names with the same key are one file there.
    fn name_key(&self, name: &str) -> String;
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

/// The SSH keys a first boot installs, as the text of their files.
#[derive(Clone, Debug, Default)]
pub(crate) struct SeedKeys {
    /// The public key of the client allowed to log in.
    pub client_public: String,
    /// The guest's SSH host key pair.
    pub host_private: String,
    pub host_public: String,
}

pub(crate) struct SeedMedia {
    pub media: Media,
}

pub(crate) struct BuildCtx {
    pub dirs: Dirs,
    pub arch: Arch,
    pub disk_size: u64,
    pub progress: Arc<dyn Progress>,
}

/// The guests compiled into this build: none yet.
pub(crate) struct BuiltinGuests;

fn missing(kind: GuestKind) -> Error {
    Error::new(
        ErrorKind::NotSupported,
        format!("{} guests are not available in this build of devvm", kind.as_str()),
    )
}

impl Guests for BuiltinGuests {
    fn os(&self, kind: GuestKind) -> Result<Arc<dyn GuestOs>> {
        Err(missing(kind))
    }

    fn builder(&self, kind: GuestKind) -> Result<Arc<dyn BaseBuilder>> {
        Err(missing(kind))
    }
}
