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

impl GuestKind {
    /// The name used in messages: `linux`, `windows` or `macos`.
    pub fn as_str(self) -> &'static str {
        match self {
            GuestKind::Linux => "linux",
            GuestKind::Windows => "windows",
            GuestKind::Macos => "macos",
        }
    }
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
}

impl Command {
    /// A command running as the ordinary user, with no working directory of its own.
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

/// Defines a digest type: `$len` bytes, parsed from `$len * 2` hex digits of either case and displayed in lowercase.
macro_rules! digest_type {
    ($(#[$doc:meta])* $name:ident, $parse_error:ident, $len:literal, $message:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name([u8; $len]);

        /// The text is not the right number of hex digits.
        #[derive(Debug, thiserror::Error)]
        #[error($message)]
        pub struct $parse_error(String);

        impl $name {
            pub fn from_bytes(bytes: [u8; $len]) -> $name {
                $name(bytes)
            }

            pub fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = $parse_error;

            fn from_str(text: &str) -> Result<$name, $parse_error> {
                let mut bytes = [0u8; $len];
                match hex::decode_to_slice(text, &mut bytes) {
                    Ok(()) => Ok($name(bytes)),
                    Err(_) => Err($parse_error(text.to_owned())),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&hex::encode(self.0))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<$name, D::Error> {
                let text = String::deserialize(deserializer)?;
                text.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

digest_type!(
    /// A SHA-256 digest. Parses 64 hex digits of either case; displays lowercase.
    Sha256,
    ParseSha256Error,
    32,
    "not a sha256 (expected 64 hex digits): `{0}`"
);

digest_type!(
    /// A SHA-512 digest. Parses 128 hex digits of either case; displays lowercase.
    Sha512,
    ParseSha512Error,
    64,
    "not a sha512 (expected 128 hex digits): `{0}`"
);

/// The digest a download is pinned by: one algorithm and its value. Displays as the hex digits alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContentHash {
    Sha256(Sha256),
    Sha512(Sha512),
}

impl ContentHash {
    /// The algorithm as a recipe names it.
    pub fn algorithm(&self) -> &'static str {
        match self {
            ContentHash::Sha256(_) => "sha256",
            ContentHash::Sha512(_) => "sha512",
        }
    }
}

impl From<Sha256> for ContentHash {
    fn from(hash: Sha256) -> ContentHash {
        ContentHash::Sha256(hash)
    }
}

impl From<Sha512> for ContentHash {
    fn from(hash: Sha512) -> ContentHash {
        ContentHash::Sha512(hash)
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContentHash::Sha256(hash) => hash.fmt(f),
            ContentHash::Sha512(hash) => hash.fmt(f),
        }
    }
}
