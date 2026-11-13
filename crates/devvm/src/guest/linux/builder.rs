//! Building the root layer of a Linux guest: the pinned cloud image, imported as the layer's disk.

use std::sync::Arc;
use std::time::SystemTime;

use secrecy::SecretString;

use super::{LinuxGuest, seed};
use crate::backend::Storage;
use crate::guest::GuestOs;
use crate::guest::{BaseBuilder, BuildCtx, SeedKeys, SeedSpec};
use crate::instance::record::InstanceId;
use crate::media::SeedFile;
use crate::recipe::Source;
use crate::recipe::hash::{Framer, LayerKey};
use crate::store::fetch::{Artifact, Fetcher};
use crate::store::layers::{Generation, LayerDir, LayerMeta};
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "builder_tests.rs"]
mod builder_tests;

/// Builds a Linux root layer. It holds what [`BuildCtx`] does not carry: the verified downloader and the storage that
/// turns the downloaded disk into a layer.
pub(crate) struct LinuxBuilder {
    fetcher: Arc<Fetcher>,
    storage: Arc<dyn Storage>,
}

impl LinuxBuilder {
    pub(crate) fn new(fetcher: Arc<Fetcher>, storage: Arc<dyn Storage>) -> LinuxBuilder {
        LinuxBuilder { fetcher, storage }
    }
}

impl BaseBuilder for LinuxBuilder {
    fn build_base(&self, src: &Source, ctx: &BuildCtx, staging: &LayerDir) -> Result<LayerMeta> {
        let Source::DiskImage(pins) = src else {
            return Err(Error::new(
                ErrorKind::NotSupported,
                format!(
                    "a Linux base is built from a pinned disk image; {} is not supported",
                    describe(src)
                ),
            ));
        };
        let pin = pins.get(ctx.arch).ok_or_else(|| {
            Error::new(
                ErrorKind::NotSupported,
                format!("the disk image has no pin for {}", ctx.arch.as_str()),
            )
        })?;
        let downloaded = self.fetcher.fetch(&Artifact {
            url: pin.url.clone(),
            sha256: pin.sha256,
        })?;
        self.storage.import_base(&downloaded, ctx.arch, staging)?;
        // The store owns the key, the generation and the creation time; it reads only the expiry from what a base
        // builder returns.
        Ok(LayerMeta {
            key: LayerKey::from_bytes([0; 16]),
            generation: Generation::from_bytes([0; 16]),
            parent: None,
            created: SystemTime::UNIX_EPOCH,
            expires: None,
        })
    }

    fn definition_hash(&self) -> [u8; 32] {
        // The seed of a fixed instance: any change to the templates or to the shape of the cloud-config changes it.
        let probe = probe_spec();
        let files = seed::seed_files(&probe).expect("the seed templates serialize");
        definition_hash_of(&files, LinuxGuest.semantics_version())
    }
}

/// The seed of a fixed instance, whose rendering [`BaseBuilder::definition_hash`] hashes.
fn probe_spec() -> SeedSpec {
    SeedSpec {
        hostname: "hostname".to_owned(),
        dir: Default::default(),
        instance_id: "00000000000000000000000000000000"
            .parse::<InstanceId>()
            .expect("32 hex digits"),
        keys: SeedKeys {
            client_public: "client-public-key".to_owned(),
            host_private: SecretString::from("host-private-key"),
            host_public: "host-public-key".to_owned(),
        },
    }
}

/// The hash of a seed definition: every file, name and contents, and the semantics version. The domain tag and the
/// field layout are those of every cached Linux base's key; a change to either invalidates them.
fn definition_hash_of(files: &[SeedFile], semantics_version: u32) -> [u8; 32] {
    let mut framer = Framer::unkeyed();
    framer.str("devvm linux base definition 1").u32(semantics_version);
    for file in files {
        framer.str(&file.name).bytes(&file.bytes);
    }
    framer.finish_digest()
}

/// A source in the words of an error message.
fn describe(source: &Source) -> String {
    match source {
        Source::Recipe(path) => format!("another recipe ({})", path.display()),
        Source::Embedded(name) => format!("the embedded recipe {name}"),
        Source::DiskImage(_) => "a disk image".to_owned(),
        Source::Oci { reference, .. } => format!("the OCI image {reference}"),
        Source::Iso { path, .. } => format!("a Windows ISO ({})", path.display()),
    }
}
