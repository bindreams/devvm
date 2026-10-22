//! Public types shared across the crate.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[cfg(test)]
#[path = "types_tests.rs"]
mod types_tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Arch {
    Amd64,
    Arm64,
}

impl Arch {
    /// The architecture this binary was compiled for.
    ///
    /// # Panics
    /// On a host architecture devvm has no guest support for.
    pub fn host() -> Arch {
        match std::env::consts::ARCH {
            "x86_64" => Arch::Amd64,
            "aarch64" => Arch::Arm64,
            other => panic!("unsupported host architecture `{other}`"),
        }
    }

    /// The name used in recipes, artifact names and messages: `amd64` or `arm64`.
    pub fn as_str(self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GuestKind {
    Linux,
    Windows,
    Macos,
}

/// Privilege a guest command runs with. `Elevated` is root via sudo on Linux and macOS, High integrity on Windows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunAs {
    #[default]
    User,
    Elevated,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Network {
    /// Guest internet through the host's NAT.
    #[default]
    Nat,
    /// No guest network beyond the host's SSH forward.
    None,
}

#[derive(Clone, Debug, Default)]
pub struct BuildOptions {
    /// Build for this architecture instead of the host's.
    pub arch: Option<Arch>,
}

/// Options for starting a VM or instance; unset fields take the defaults (2 CPUs, 4 GiB).
#[derive(Clone, Debug, Default)]
pub struct StartOptions {
    pub cpus: Option<u32>,
    /// Bytes.
    pub memory: Option<u64>,
    pub network: Network,
}

#[derive(Clone, Debug, Default)]
pub struct Command {
    pub argv: Vec<String>,
    pub run_as: RunAs,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
}

impl Command {
    /// A command running as the ordinary user, with no working directory or environment of its own.
    pub fn new<I, S>(argv: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Command {
            argv: argv.into_iter().map(Into::into).collect(),
            ..Command::default()
        }
    }
}

/// A SHA-256 digest. Parses 64 hex digits of either case; displays lowercase.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256([u8; 32]);

/// The text is not 64 hex digits.
#[derive(Debug, thiserror::Error)]
#[error("not a sha256 (expected 64 hex digits): `{0}`")]
pub struct ParseSha256Error(String);

impl Sha256 {
    pub fn from_bytes(bytes: [u8; 32]) -> Sha256 {
        Sha256(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl FromStr for Sha256 {
    type Err = ParseSha256Error;

    fn from_str(text: &str) -> Result<Sha256, ParseSha256Error> {
        let mut bytes = [0u8; 32];
        match hex::decode_to_slice(text, &mut bytes) {
            Ok(()) => Ok(Sha256(bytes)),
            Err(_) => Err(ParseSha256Error(text.to_owned())),
        }
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256({self})")
    }
}

impl Serialize for Sha256 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Sha256 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Sha256, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}
