//! Guest operating systems: what differs between Linux, Windows and macOS guests, behind traits. U4 and U5 add the
//! implementations.

use std::path::PathBuf;
use std::sync::Arc;

use crate::access::SshSession;
use crate::backend::{DeviceProfile, Media};
use crate::exit::Progress;
use crate::recipe::Source;
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

/// The guests compiled into this build: none yet. U4 adds Linux and U5 adds Windows.
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
