//! Layer keys: a content address for every layer of a recipe chain.
//!
//! A key is the first 128 bits of a SHA-256 over length-prefixed fields, so it is identical in every process and on
//! every host OS. Everything that can change a layer's bytes is hashed; nothing else is (a recipe's comments, key
//! order and line endings never reach this module because keys are computed from the parsed [`Step`]s).

use std::fmt;
use std::str::FromStr;

use sha2::{Digest, Sha256 as Sha2};

use super::{Flattened, Setting, Shell, Step, UacMode};
use crate::{Arch, Error, ErrorKind, Result, RunAs, Sha256};

#[cfg(test)]
#[path = "hash_tests.rs"]
mod hash_tests;

/// Bumped whenever core `copy` or `run` behavior changes in a way that changes a layer's bytes.
pub const STEP_SEMANTICS_VERSION: u32 = 1;

const DOMAIN: &str = "devvm.layer-key.v1";

// Keys ================================================================================================================

/// The identity of one layer: 128 bits, shown as 32 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LayerKey([u8; 16]);

impl LayerKey {
    pub fn from_bytes(bytes: [u8; 16]) -> LayerKey {
        LayerKey(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for LayerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for LayerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LayerKey({self})")
    }
}

/// The text is not 32 hex digits.
#[derive(Debug, thiserror::Error)]
#[error("not a layer key (expected 32 hex digits): `{0}`")]
pub struct ParseKeyError(String);

impl FromStr for LayerKey {
    type Err = ParseKeyError;

    fn from_str(text: &str) -> std::result::Result<LayerKey, ParseKeyError> {
        let mut bytes = [0u8; 16];
        hex::decode_to_slice(text, &mut bytes).map_err(|_| ParseKeyError(text.to_owned()))?;
        Ok(LayerKey(bytes))
    }
}

/// Inputs shared by every layer of one chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyContext {
    pub arch: Arch,
    /// The chain's disk size in bytes (the leaf recipe's), or `None` for the default.
    pub disk: Option<u64>,
    /// The guest's `GuestOs::semantics_version()`.
    pub guest_semantics_version: u32,
}

/// What a root layer is made from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootContent {
    /// The sha256 of a disk image or of an ISO's content.
    Sha256(Sha256),
    /// An OCI image digest; hashed in lowercase, so case variants share a key.
    OciDigest(String),
}

// Framing =============================================================================================================

/// The only place that frames bytes for hashing: every field is an 8-byte little-endian length and the bytes, so two
/// different field sequences never produce the same input.
struct Framer(Sha2);

impl Framer {
    fn new(kind: &str) -> Framer {
        let mut framer = Framer(Sha2::new());
        framer.str(DOMAIN);
        framer.str(kind);
        framer
    }

    fn bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update((bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
        self
    }

    fn str(&mut self, text: &str) -> &mut Self {
        self.bytes(text.as_bytes())
    }

    /// A numeric tag assigned in this file, not a Rust discriminant.
    fn tag(&mut self, tag: u8) -> &mut Self {
        self.bytes(&[tag])
    }

    fn u32(&mut self, value: u32) -> &mut Self {
        self.bytes(&value.to_le_bytes())
    }

    fn u64(&mut self, value: u64) -> &mut Self {
        self.bytes(&value.to_le_bytes())
    }

    fn opt_u64(&mut self, value: Option<u64>) -> &mut Self {
        match value {
            None => self.tag(0),
            Some(value) => self.tag(1).u64(value),
        }
    }

    fn opt_str(&mut self, value: Option<&str>) -> &mut Self {
        match value {
            None => self.tag(0),
            Some(value) => self.tag(1).str(value),
        }
    }

    fn finish(self) -> LayerKey {
        let digest = self.0.finalize();
        let mut key = [0u8; 16];
        key.copy_from_slice(&digest[..16]);
        LayerKey(key)
    }

    fn context(&mut self, ctx: &KeyContext, step_semantics: u32) -> &mut Self {
        let arch = match ctx.arch {
            Arch::Amd64 => 1,
            Arch::Arm64 => 2,
        };
        self.tag(arch)
            .opt_u64(ctx.disk)
            .u32(step_semantics)
            .u32(ctx.guest_semantics_version)
    }

    fn step(&mut self, step: &Step) -> &mut Self {
        match step {
            Step::Run {
                command,
                shell,
                run_as,
                cwd,
            } => {
                self.tag(1).str(command);
                match shell {
                    None => self.tag(0),
                    Some(Shell::Sh) => self.tag(1),
                    Some(Shell::Bash) => self.tag(2),
                    Some(Shell::Pwsh) => self.tag(3),
                    Some(Shell::Cmd) => self.tag(4),
                };
                match run_as {
                    RunAs::User => self.tag(0),
                    RunAs::Elevated => self.tag(1),
                };
                self.opt_str(cwd.as_deref())
            }
            Step::Copy { from, to, read_only } => {
                match from {
                    super::CopyFrom::GitTracked => self.tag(2).tag(1),
                };
                self.str(to).tag(u8::from(*read_only))
            }
            Step::Set(Setting::Uac(UacMode::AutoApprove)) => self.tag(3).tag(1).tag(1),
        }
    }

    fn manifest(&mut self, manifest: &CopyManifest) -> &mut Self {
        self.u64(manifest.entries.len() as u64);
        for entry in &manifest.entries {
            self.str(&entry.path).u32(entry.mode);
            match &entry.content {
                EntryContent::FileSha256(sha) => self.tag(1).bytes(sha.as_bytes()),
                EntryContent::SymlinkTarget(target) => self.tag(2).str(target),
            };
        }
        self
    }
}

// Keys of a chain =====================================================================================================

/// The key of a root layer. `base_definition` is the hash of devvm's base-build definition for the guest kind.
pub fn root_key(ctx: &KeyContext, content: &RootContent, base_definition: &[u8; 32]) -> LayerKey {
    root_key_with(ctx, content, base_definition, STEP_SEMANTICS_VERSION)
}

pub(crate) fn root_key_with(
    ctx: &KeyContext,
    content: &RootContent,
    base_definition: &[u8; 32],
    step_semantics: u32,
) -> LayerKey {
    let mut framer = Framer::new("root");
    framer.context(ctx, step_semantics);
    match content {
        RootContent::Sha256(sha) => framer.tag(1).bytes(sha.as_bytes()),
        RootContent::OciDigest(digest) => framer.tag(2).str(&digest.to_ascii_lowercase()),
    };
    framer.bytes(base_definition);
    framer.finish()
}

/// The key of the layer a `step` produces on top of `parent`. A `copy` step needs its `manifest`; no other step
/// takes one (`Internal` otherwise).
pub fn step_key(ctx: &KeyContext, parent: &LayerKey, step: &Step, manifest: Option<&CopyManifest>) -> Result<LayerKey> {
    step_key_with(ctx, parent, step, manifest, STEP_SEMANTICS_VERSION)
}

pub(crate) fn step_key_with(
    ctx: &KeyContext,
    parent: &LayerKey,
    step: &Step,
    manifest: Option<&CopyManifest>,
    step_semantics: u32,
) -> Result<LayerKey> {
    match (step, manifest) {
        (Step::Copy { .. }, None) => {
            return Err(Error::new(
                ErrorKind::Internal,
                "a copy step needs a manifest to be keyed",
            ));
        }
        (Step::Run { .. } | Step::Set(_), Some(_)) => {
            return Err(Error::new(ErrorKind::Internal, "only a copy step takes a manifest"));
        }
        _ => {}
    }
    let mut framer = Framer::new("step");
    framer.context(ctx, step_semantics).bytes(parent.as_bytes()).step(step);
    if let Some(manifest) = manifest {
        framer.manifest(manifest);
    }
    Ok(framer.finish())
}

/// The keys of a whole chain: the root's, then one per step. `manifests` is parallel to `flat.steps`.
pub fn chain_keys(
    flat: &Flattened,
    ctx: &KeyContext,
    content: &RootContent,
    base_definition: &[u8; 32],
    manifests: &[Option<CopyManifest>],
) -> Result<Vec<LayerKey>> {
    if manifests.len() != flat.steps.len() {
        return Err(Error::new(
            ErrorKind::Internal,
            format!("{} manifests for {} steps", manifests.len(), flat.steps.len()),
        ));
    }
    debug_assert_eq!(
        ctx.disk, flat.resources.disk,
        "the key context must carry the chain's disk size"
    );
    let mut keys = Vec::with_capacity(flat.steps.len() + 1);
    keys.push(root_key(ctx, content, base_definition));
    for (flat_step, manifest) in flat.steps.iter().zip(manifests) {
        let parent = *keys.last().expect("the root key is first");
        keys.push(step_key(ctx, &parent, &flat_step.step, manifest.as_ref())?);
    }
    Ok(keys)
}

// Copy manifests ======================================================================================================

const MODE_FILE: u32 = 0o100644;
const MODE_EXECUTABLE: u32 = 0o100755;
const MODE_SYMLINK: u32 = 0o120000;

/// What a copied path holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryContent {
    FileSha256(Sha256),
    SymlinkTarget(String),
}

/// One copied path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestEntry {
    /// Relative, `/`-separated.
    pub path: String,
    /// The git mode: `0o100644`, `0o100755` or `0o120000`.
    pub mode: u32,
    pub content: EntryContent,
}

/// The files a `copy` step transfers, sorted by path bytes. It is what the layer key hashes, so the key follows the
/// bytes that get copied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyManifest {
    entries: Vec<ManifestEntry>,
}

impl CopyManifest {
    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    /// Sorts the entries. Rejects (kind `Recipe`) duplicate paths, absolute or empty paths, empty components, `.` and
    /// `..` components, a trailing slash, backslashes, NUL, a mode git does not use, and a mode that disagrees with
    /// the content kind.
    pub fn new(mut entries: Vec<ManifestEntry>) -> Result<CopyManifest> {
        for entry in &entries {
            check_entry(entry)?;
        }
        entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        if let Some(pair) = entries.windows(2).find(|pair| pair[0].path == pair[1].path) {
            return Err(rejected(&pair[0].path, "listed twice"));
        }
        Ok(CopyManifest { entries })
    }
}

fn rejected(path: &str, why: &str) -> Error {
    Error::new(
        ErrorKind::Recipe,
        format!("cannot copy `{}`: {why}", path.escape_debug()),
    )
}

fn check_entry(entry: &ManifestEntry) -> Result<()> {
    let path = entry.path.as_str();
    if path.is_empty() {
        return Err(rejected(path, "empty path"));
    }
    if path.contains('\\') || path.contains('\0') {
        return Err(rejected(path, "backslash or NUL in path"));
    }
    for component in path.split('/') {
        match component {
            "" => return Err(rejected(path, "absolute path, empty component or trailing slash")),
            "." | ".." => return Err(rejected(path, "`.` or `..` component")),
            _ => {}
        }
    }
    let consistent = matches!(
        (&entry.content, entry.mode),
        (EntryContent::FileSha256(_), MODE_FILE | MODE_EXECUTABLE) | (EntryContent::SymlinkTarget(_), MODE_SYMLINK)
    );
    if !consistent {
        return Err(rejected(
            path,
            &format!("mode {:o} does not fit its content", entry.mode),
        ));
    }
    Ok(())
}
