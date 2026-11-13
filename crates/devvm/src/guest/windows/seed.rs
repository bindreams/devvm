//! The per-instance seed of a Windows guest: the files the boot task reads from the `CIDATA` disk and applies.

use secrecy::ExposeSecret;
use serde::Serialize;

use crate::guest::SeedSpec;
use crate::media::SeedFile;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "seed_tests.rs"]
mod seed_tests;

#[derive(Serialize)]
struct MetaData {
    #[serde(rename = "instance-id")]
    instance_id: String,
}

/// `meta.json`, the host key pair and `authorized_keys` for `spec`. A pure function of `spec`: the boot task writes
/// the keys at every boot, so a stopped instance presents the same host key when it starts again.
pub(super) fn seed_files(spec: &SeedSpec) -> Result<Vec<SeedFile>> {
    spec.keys.require_present()?;
    let meta = serde_json::to_string_pretty(&MetaData {
        instance_id: format!("devvm-{}", spec.instance_id),
    })
    .map_err(|e| Error::new(ErrorKind::Internal, format!("cannot write the seed's meta.json: {e}")))?;
    let file = |name: &str, text: &str| SeedFile {
        name: name.to_owned(),
        bytes: terminated(text).into_bytes(),
    };
    Ok(vec![
        file("meta.json", &meta),
        file("ssh_host_ed25519_key", spec.keys.host_private.expose_secret()),
        file("ssh_host_ed25519_key.pub", &spec.keys.host_public),
        file("authorized_keys", &spec.keys.client_public),
    ])
}

/// `text` with a final line feed: OpenSSH refuses a key file whose last line is unterminated.
fn terminated(text: &str) -> String {
    if text.ends_with('\n') {
        text.to_owned()
    } else {
        format!("{text}\n")
    }
}
