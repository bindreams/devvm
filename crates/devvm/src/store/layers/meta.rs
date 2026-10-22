//! `meta.json`: the record every published generation carries, and the identifiers it is made of.

use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::recipe::hash::LayerKey;
use crate::{Error, ErrorKind, Result};

/// The `meta.json` schema this devvm writes and reads.
pub const SCHEMA: u64 = 1;

/// A rebuild of a layer key gets a new one; 128 random bits, shown as 32 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Generation([u8; 16]);

impl Generation {
    pub fn from_bytes(bytes: [u8; 16]) -> Generation {
        Generation(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// A fresh random generation.
    pub fn random() -> Generation {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("the OS random number generator is available");
        Generation(bytes)
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Generation({self})")
    }
}

/// The text is not 32 hex digits.
#[derive(Debug, thiserror::Error)]
#[error("not a generation (expected 32 hex digits): `{0}`")]
pub struct ParseGenerationError(String);

impl FromStr for Generation {
    type Err = ParseGenerationError;

    fn from_str(text: &str) -> std::result::Result<Generation, ParseGenerationError> {
        let mut bytes = [0u8; 16];
        hex::decode_to_slice(text, &mut bytes).map_err(|_| ParseGenerationError(text.to_owned()))?;
        Ok(Generation(bytes))
    }
}

/// One generation of one layer key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LayerRef {
    pub key: LayerKey,
    pub generation: Generation,
}

/// The record of a published layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerMeta {
    pub key: LayerKey,
    pub generation: Generation,
    pub parent: Option<LayerRef>,
    pub created: SystemTime,
    pub expires: Option<SystemTime>,
}

// On disk =============================================================================================================

#[derive(Serialize, Deserialize)]
struct WireRef {
    key: String,
    generation: String,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    schema: u64,
    key: String,
    generation: String,
    parent: Option<WireRef>,
    created: String,
    expires: Option<String>,
}

/// Reads only `schema`, so a record written by a newer devvm is recognized whatever else it holds.
#[derive(Deserialize)]
struct SchemaOnly {
    schema: u64,
}

/// What reading `meta.json` found.
pub enum MetaRead {
    Absent,
    /// Present but unusable; the text says why.
    Corrupt(String),
    Valid(LayerMeta),
}

fn time_text(time: SystemTime) -> Result<String> {
    jiff::Timestamp::try_from(time)
        .map(|timestamp| timestamp.to_string())
        .map_err(|e| Error::new(ErrorKind::Store, format!("time out of range: {e}")))
}

fn time_parse(text: &str) -> std::result::Result<SystemTime, String> {
    text.parse::<jiff::Timestamp>()
        .map(SystemTime::from)
        .map_err(|e| format!("bad time `{text}`: {e}"))
}

impl LayerMeta {
    pub(super) fn to_json(&self) -> Result<String> {
        let wire = Wire {
            schema: SCHEMA,
            key: self.key.to_string(),
            generation: self.generation.to_string(),
            parent: self.parent.map(|parent| WireRef {
                key: parent.key.to_string(),
                generation: parent.generation.to_string(),
            }),
            created: time_text(self.created)?,
            expires: self.expires.map(time_text).transpose()?,
        };
        serde_json::to_string_pretty(&wire).map_err(|e| Error::new(ErrorKind::Internal, e.to_string()))
    }
}

fn from_wire(wire: Wire) -> std::result::Result<LayerMeta, String> {
    let key = wire.key.parse::<LayerKey>().map_err(|e| e.to_string())?;
    let generation = wire.generation.parse::<Generation>().map_err(|e| e.to_string())?;
    let parent = match wire.parent {
        None => None,
        Some(parent) => Some(LayerRef {
            key: parent.key.parse::<LayerKey>().map_err(|e| e.to_string())?,
            generation: parent.generation.parse::<Generation>().map_err(|e| e.to_string())?,
        }),
    };
    Ok(LayerMeta {
        key,
        generation,
        parent,
        created: time_parse(&wire.created)?,
        expires: wire.expires.as_deref().map(time_parse).transpose()?,
    })
}

/// Reads `meta.json` from a generation directory. A newer schema is an error, never a "corrupt" record.
pub fn read(generation_dir: &Path) -> Result<MetaRead> {
    let path = generation_dir.join("meta.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(MetaRead::Absent),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            return Ok(MetaRead::Corrupt(format!("not UTF-8: {e}")));
        }
        Err(e) => return Err(Error::io(ErrorKind::Io, format!("cannot read {}", path.display()), e)),
    };
    let schema = match serde_json::from_str::<SchemaOnly>(&text) {
        Ok(schema) => schema.schema,
        Err(e) => return Ok(MetaRead::Corrupt(e.to_string())),
    };
    if schema > SCHEMA {
        return Err(Error::new(
            ErrorKind::Store,
            format!(
                "{} has schema {schema}, written by a newer devvm than this one (schema {SCHEMA}); upgrade devvm",
                path.display()
            ),
        ));
    }
    if schema == 0 {
        return Ok(MetaRead::Corrupt("schema 0".into()));
    }
    let wire = match serde_json::from_str::<Wire>(&text) {
        Ok(wire) => wire,
        Err(e) => return Ok(MetaRead::Corrupt(e.to_string())),
    };
    Ok(match from_wire(wire) {
        Ok(meta) => MetaRead::Valid(meta),
        Err(why) => MetaRead::Corrupt(why),
    })
}
