//! The per-instance SSH keys: the client key that logs in and the host key the guest presents.

use std::path::Path;

use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey};

use crate::exit::NullProgress;
use crate::host::Host;
use crate::store::lock::{FileLock, Mode};
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "keys_tests.rs"]
mod keys_tests;

const CLIENT: &str = "client";
const HOST: &str = "host";
const LOCK: &str = ".keys.lock";
const TEMP_PREFIX: &str = ".keys-";

/// An instance's key pairs as OpenSSH text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InstanceKeys {
    pub client_private: String,
    pub client_public: String,
    pub host_private: String,
    pub host_public: String,
}

fn key_error(what: &str, source: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::new(ErrorKind::Internal, format!("cannot {what}: {source}")).with_source(source)
}

/// A fresh ed25519 pair: the private key and the public key, both as OpenSSH text.
fn generate_pair() -> Result<(String, String)> {
    let private = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .map_err(|e| key_error("generate an ed25519 key", e))?;
    let public = private
        .public_key()
        .to_openssh()
        .map_err(|e| key_error("encode a public key", e))?;
    let private = private
        .to_openssh(LineEnding::LF)
        .map_err(|e| key_error("encode a private key", e))?;
    Ok((private.to_string(), public))
}

/// Removes the `.keys-*` directories in `parent`: every writer holds the lock, so none of them is in use.
fn sweep_temporaries(parent: &Path) -> Result<()> {
    let io =
        |what: &str, path: &Path, source| Error::io(ErrorKind::Io, format!("cannot {what} {}", path.display()), source);
    for entry in std::fs::read_dir(parent).map_err(|e| io("list", parent, e))? {
        let entry = entry.map_err(|e| io("list", parent, e))?;
        if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
            let path = entry.path();
            std::fs::remove_dir_all(&path).map_err(|e| io("remove the leftover", &path, e))?;
        }
    }
    Ok(())
}

impl InstanceKeys {
    /// Two new ed25519 pairs; no two instances share one.
    pub(crate) fn generate() -> Result<InstanceKeys> {
        let (client_private, client_public) = generate_pair()?;
        let (host_private, host_public) = generate_pair()?;
        Ok(InstanceKeys {
            client_private,
            client_public,
            host_private,
            host_public,
        })
    }

    /// Writes `client`, `client.pub`, `host` and `host.pub` into the new directory `dir`. The files are first written
    /// to a temporary directory beside `dir`, which is then renamed into place: a failure or a kill leaves no
    /// `dir`, and an existing `dir` is `AlreadyExists`, never replaced. Writers of one parent directory take turns
    /// under a lock there (`.keys.lock`), so of several at once exactly one wins, and a temporary directory a killed
    /// writer left (`.keys-*`) is removed by the next. The private files go through [`Host::write_private`].
    pub(crate) fn write(&self, host: &dyn Host, dir: &Path) -> Result<()> {
        let io = |what: &str, path: &Path, source| {
            Error::io(ErrorKind::Io, format!("cannot {what} {}", path.display()), source)
        };
        let parent = dir.parent().ok_or_else(|| {
            Error::new(
                ErrorKind::Internal,
                format!("{} has no parent directory", dir.display()),
            )
        })?;
        std::fs::create_dir_all(parent).map_err(|e| io("create", parent, e))?;
        let mut lock = FileLock::open(&parent.join(LOCK))?;
        lock.lock(Mode::Exclusive, "the instance keys", &NullProgress)?;
        if dir.exists() {
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                format!("the keys directory {} already exists", dir.display()),
            ));
        }
        sweep_temporaries(parent)?;
        // Removed on drop unless it is renamed into place.
        let temp = tempfile::Builder::new()
            .prefix(TEMP_PREFIX)
            .tempdir_in(parent)
            .map_err(|e| io("create a temporary directory in", parent, e))?;
        let files = [
            (CLIENT, &self.client_private, true),
            ("client.pub", &self.client_public, false),
            (HOST, &self.host_private, true),
            ("host.pub", &self.host_public, false),
        ];
        for (name, text, private) in files {
            let path = temp.path().join(name);
            let written = if private {
                host.write_private(&path, text.as_bytes())
            } else {
                std::fs::write(&path, text)
            };
            written.map_err(|e| io("write", &path, e))?;
        }
        let temp = temp.keep();
        std::fs::rename(&temp, dir).map_err(|e| {
            let _ = std::fs::remove_dir_all(&temp);
            io("move the keys into place at", dir, e)
        })
    }

    /// Reads the set `write` made. `NotFound` when `dir` has no keys.
    pub(crate) fn read(dir: &Path) -> Result<InstanceKeys> {
        let read = |name: &str| -> Result<String> {
            let path = dir.join(name);
            std::fs::read_to_string(&path).map_err(|e| {
                let kind = if e.kind() == std::io::ErrorKind::NotFound {
                    ErrorKind::NotFound
                } else {
                    ErrorKind::Io
                };
                Error::io(kind, format!("cannot read the key {}", path.display()), e)
            })
        };
        Ok(InstanceKeys {
            client_private: read(CLIENT)?,
            client_public: read("client.pub")?,
            host_private: read(HOST)?,
            host_public: read("host.pub")?,
        })
    }
}
