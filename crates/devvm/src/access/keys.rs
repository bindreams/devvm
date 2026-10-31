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

/// Removes the `.keys-*` directories in `parent`: every writer holds the lock, so none of them is in use. One that
/// cannot be removed (an antivirus holds a file of it, or it is not a directory) is returned as a warning: it must not
/// stop every later write.
fn sweep_temporaries(parent: &Path) -> Result<Vec<String>> {
    let io =
        |what: &str, path: &Path, source| Error::io(ErrorKind::Io, format!("cannot {what} {}", path.display()), source);
    let mut warnings = Vec::new();
    for entry in std::fs::read_dir(parent).map_err(|e| io("list", parent, e))? {
        let entry = entry.map_err(|e| io("list", parent, e))?;
        if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
            let path = entry.path();
            if let Err(e) = std::fs::remove_dir_all(&path) {
                warnings.push(io("remove the leftover", &path, e).message().to_owned());
            }
        }
    }
    Ok(warnings)
}

/// Creates the public key file `path` with `text` and flushes it to disk.
fn write_public(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create_new(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
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
    /// to a temporary directory beside `dir` and flushed to disk, which is then published at `dir` in one step
    /// ([`Host::publish_dir`]): a failure or a kill leaves no `dir`, and anything already at `dir` is `AlreadyExists`,
    /// never replaced. Writers of one parent directory take turns under a lock there (`.keys.lock`), so a temporary
    /// directory a killed writer left (`.keys-*`) is removed by the next; one that cannot be removed is returned as a
    /// warning. The private files go through [`Host::write_private`].
    pub(crate) fn write(&self, host: &dyn Host, dir: &Path) -> Result<Vec<String>> {
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
        let warnings = sweep_temporaries(parent)?;
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
        let filled = || -> Result<()> {
            for (name, text, private) in files {
                let path = temp.path().join(name);
                let written = if private {
                    host.write_private(&path, text.as_bytes())
                } else {
                    write_public(&path, text)
                };
                written.map_err(|e| io("write", &path, e))?;
            }
            host.sync_dir(temp.path()).map_err(|e| io("flush", temp.path(), e))
        };
        if let Err(e) = filled() {
            let left = format!("the temporary directory {}", temp.path().display());
            return Err(e.after_cleanup(&left, temp.close()));
        }
        let temp = temp.keep();
        if let Err(e) = host.publish_dir(&temp, dir) {
            let kind = if e.kind() == std::io::ErrorKind::AlreadyExists {
                ErrorKind::AlreadyExists
            } else {
                ErrorKind::Io
            };
            let published = Error::io(kind, format!("cannot move the keys into place at {}", dir.display()), e);
            let left = format!("the temporary directory {}", temp.display());
            return Err(published.after_cleanup(&left, std::fs::remove_dir_all(&temp)));
        }
        host.sync_dir(parent).map_err(|e| io("flush", parent, e))?;
        Ok(warnings)
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
