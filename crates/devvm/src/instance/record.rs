//! `record.json` (what an instance is) and `owner.json` (who keeps it alive): the files an instance directory holds.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::backend::MachineRecord;
use crate::host::remove_if_present;
use crate::instance::name::InstanceName;
use crate::recipe::hash::LayerKey;
use crate::store::layers::{Generation, LayerRef};
use crate::{Arch, Error, ErrorKind, GuestKind, Network, Result};

#[cfg(test)]
#[path = "record_tests.rs"]
mod record_tests;

/// The schema of `record.json` and `owner.json` this devvm writes and reads.
pub(crate) const SCHEMA: u64 = 1;

const RECORD_FILE: &str = "record.json";
const MARKER_FILE: &str = "owner.json";

// Ids =================================================================================================================

/// The random identity of one instance, shown as 32 lowercase hex digits. A name can be reused; an id never is.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct InstanceId([u8; 16]);

impl InstanceId {
    pub(crate) fn random() -> InstanceId {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("the OS random number generator is available");
        InstanceId(bytes)
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InstanceId({self})")
    }
}

/// The text is not 32 lowercase hex digits.
#[derive(Debug, thiserror::Error)]
#[error("not an instance id (expected 32 lowercase hex digits): `{0}`")]
pub(crate) struct ParseIdError(String);

impl FromStr for InstanceId {
    type Err = ParseIdError;

    fn from_str(text: &str) -> std::result::Result<InstanceId, ParseIdError> {
        let mut bytes = [0u8; 16];
        let lowercase = !text.bytes().any(|b| b.is_ascii_uppercase());
        match hex::decode_to_slice(text, &mut bytes) {
            Ok(()) if lowercase => Ok(InstanceId(bytes)),
            _ => Err(ParseIdError(text.to_owned())),
        }
    }
}

// The record ==========================================================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InstanceRecord {
    pub id: InstanceId,
    pub name: InstanceName,
    pub created: SystemTime,
    /// The directory the instance was created from; `ls` filters on it.
    pub created_from: PathBuf,
    pub guest: GuestKind,
    pub arch: Arch,
    /// Every ancestor layer, root first. This is what keeps the generations alive.
    pub chain: Vec<LayerRef>,
    pub expires: Option<SystemTime>,
    pub cpus: u32,
    pub memory: u64,
    pub network: Network,
    /// How many launches have been started for this instance: a launch takes the next number, under the op lock and
    /// before it spawns, and a number is never taken twice. A machine's files are named by it, so no two machines of an
    /// instance share a path.
    pub launches: u64,
    pub machine: Option<MachineRecord>,
}

/// What reading a record found.
pub(crate) enum RecordRead {
    Absent,
    /// Present but unusable; the text says why.
    Corrupt(String),
    Valid(Box<InstanceRecord>),
}

#[derive(Serialize, Deserialize)]
struct WireRef {
    key: String,
    generation: String,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    schema: u64,
    id: String,
    name: String,
    created: String,
    created_from: String,
    guest: String,
    arch: String,
    chain: Vec<WireRef>,
    expires: Option<String>,
    cpus: u32,
    memory: u64,
    network: String,
    /// Absent in a record written before launches were counted.
    #[serde(default)]
    launches: u64,
    machine: Option<MachineRecord>,
}

/// Reads only `schema`, so a file written by a newer devvm is recognized whatever else it holds.
#[derive(Deserialize)]
struct SchemaOnly {
    schema: u64,
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

fn guest_parse(text: &str) -> std::result::Result<GuestKind, String> {
    [GuestKind::Linux, GuestKind::Windows, GuestKind::Macos]
        .into_iter()
        .find(|kind| kind.as_str() == text)
        .ok_or_else(|| format!("unknown guest `{text}`"))
}

fn arch_parse(text: &str) -> std::result::Result<Arch, String> {
    [Arch::Amd64, Arch::Arm64]
        .into_iter()
        .find(|arch| arch.as_str() == text)
        .ok_or_else(|| format!("unknown architecture `{text}`"))
}

fn network_text(network: Network) -> &'static str {
    match network {
        Network::Nat => "nat",
        Network::None => "none",
    }
}

fn network_parse(text: &str) -> std::result::Result<Network, String> {
    match text {
        "nat" => Ok(Network::Nat),
        "none" => Ok(Network::None),
        other => Err(format!("unknown network `{other}`")),
    }
}

fn ref_parse(wire: &WireRef) -> std::result::Result<LayerRef, String> {
    Ok(LayerRef {
        key: wire.key.parse::<LayerKey>().map_err(|e| e.to_string())?,
        generation: wire.generation.parse::<Generation>().map_err(|e| e.to_string())?,
    })
}

fn from_wire(wire: Wire) -> std::result::Result<InstanceRecord, String> {
    if wire.chain.is_empty() {
        return Err("the layer chain is empty".to_owned());
    }
    Ok(InstanceRecord {
        id: wire.id.parse::<InstanceId>().map_err(|e| e.to_string())?,
        name: InstanceName::parse(&wire.name)
            .map_err(|e| e.message().to_owned())
            .and_then(|name| {
                if name.as_str() == wire.name {
                    Ok(name)
                } else {
                    Err(format!("the name `{}` is not lowercase", wire.name))
                }
            })?,
        created: time_parse(&wire.created)?,
        created_from: PathBuf::from(wire.created_from),
        guest: guest_parse(&wire.guest)?,
        arch: arch_parse(&wire.arch)?,
        chain: wire
            .chain
            .iter()
            .map(ref_parse)
            .collect::<std::result::Result<_, _>>()?,
        expires: wire.expires.as_deref().map(time_parse).transpose()?,
        cpus: wire.cpus,
        memory: wire.memory,
        network: network_parse(&wire.network)?,
        launches: wire.launches,
        machine: wire.machine,
    })
}

/// Reads `file` in `dir`, parses it with `parse` and maps a newer schema to `Err(Store)`.
fn read_file<T>(
    dir: &Path,
    file: &str,
    parse: impl FnOnce(&str) -> std::result::Result<T, String>,
) -> Result<std::result::Result<Option<T>, String>> {
    let path = dir.join(file);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Ok(None)),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            return Ok(Err(format!("not UTF-8: {e}")));
        }
        Err(e) => return Err(Error::io(ErrorKind::Io, format!("cannot read {}", path.display()), e)),
    };
    let schema = match serde_json::from_str::<SchemaOnly>(&text) {
        Ok(schema) => schema.schema,
        Err(e) => return Ok(Err(e.to_string())),
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
        return Ok(Err("schema 0".to_owned()));
    }
    Ok(parse(&text).map(Some))
}

/// What [`write_file`] appends to a name for the file it writes first, followed by [`TEMP_DIGITS`] hex digits.
const TEMP_MARK: &str = ".tmp-";
const TEMP_DIGITS: usize = 8;

/// Whether `name` is the temporary file of a [`write_file`] that was killed before its rename.
pub(crate) fn is_write_file_temporary(name: &str) -> bool {
    name.rsplit_once(TEMP_MARK).is_some_and(|(base, digits)| {
        !base.is_empty()
            && digits.len() == TEMP_DIGITS
            && digits.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Writes `text` to `dir/file` through a temp file and a rename, so a reader sees the old or the new content, never a
/// mixture, and a crash leaves no half-written `file`.
pub(crate) fn write_file(dir: &Path, file: &str, text: &str) -> Result<()> {
    use std::io::Write;
    let mut suffix = [0u8; TEMP_DIGITS / 2];
    getrandom::fill(&mut suffix).expect("the OS random number generator is available");
    let temp = dir.join(format!("{file}{TEMP_MARK}{}", hex::encode(suffix)));
    let target = dir.join(file);
    let fail = |action: &str, path: &Path, source| {
        Error::io(ErrorKind::Io, format!("cannot {action} {}", path.display()), source)
    };
    (|| {
        let mut out = std::fs::File::create_new(&temp).map_err(|e| fail("create", &temp, e))?;
        out.write_all(text.as_bytes()).map_err(|e| fail("write", &temp, e))?;
        out.sync_all().map_err(|e| fail("sync", &temp, e))?;
        drop(out);
        std::fs::rename(&temp, &target).map_err(|e| fail("replace", &target, e))
    })()
    .map_err(|e| {
        e.after_cleanup(
            &format!("the temporary file {}", temp.display()),
            remove_if_present(&temp),
        )
    })
}

impl InstanceRecord {
    /// Reads `record.json` in `dir`. A file from a newer devvm is an `Err(Store)`, never `Corrupt`: it is somebody
    /// else's live instance.
    pub(crate) fn read(dir: &Path) -> Result<RecordRead> {
        let parse = |text: &str| -> std::result::Result<InstanceRecord, String> {
            let wire = serde_json::from_str::<Wire>(text).map_err(|e| e.to_string())?;
            from_wire(wire)
        };
        Ok(match read_file(dir, RECORD_FILE, parse)? {
            Ok(Some(record)) => RecordRead::Valid(Box::new(record)),
            Ok(None) => RecordRead::Absent,
            Err(why) => RecordRead::Corrupt(why),
        })
    }

    /// Writes `record.json` in `dir` atomically. A `created_from` that is not valid Unicode cannot be stored and is
    /// refused with an error naming it.
    pub(crate) fn write(&self, dir: &Path) -> Result<()> {
        let created_from = self.created_from.to_str().ok_or_else(|| {
            Error::new(
                ErrorKind::Io,
                format!(
                    "the working directory {} is not valid Unicode, so it cannot be recorded",
                    self.created_from.display()
                ),
            )
        })?;
        let wire = Wire {
            schema: SCHEMA,
            id: self.id.to_string(),
            name: self.name.as_str().to_owned(),
            created: time_text(self.created)?,
            created_from: created_from.to_owned(),
            guest: self.guest.as_str().to_owned(),
            arch: self.arch.as_str().to_owned(),
            chain: self
                .chain
                .iter()
                .map(|layer| WireRef {
                    key: layer.key.to_string(),
                    generation: layer.generation.to_string(),
                })
                .collect(),
            expires: self.expires.map(time_text).transpose()?,
            cpus: self.cpus,
            memory: self.memory,
            network: network_text(self.network).to_owned(),
            launches: self.launches,
            machine: self.machine.clone(),
        };
        let text = serde_json::to_string_pretty(&wire).map_err(|e| Error::new(ErrorKind::Internal, e.to_string()))?;
        write_file(dir, RECORD_FILE, &text)
    }
}

// The owner marker ====================================================================================================

/// Who keeps an instance alive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    /// A devvm process: the instance goes when its owner lock is free.
    Process,
    /// Nobody: the instance stays until `rm`.
    None,
}

/// `owner.json`: the owner and the instance id, so the reaper can lock an instance whose record is unreadable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Marker {
    pub owner: Owner,
    pub id: InstanceId,
}

pub(crate) enum MarkerRead {
    Absent,
    /// Present but unusable (torn, empty, hand-edited); the text says why.
    Corrupt(String),
    Valid(Marker),
}

#[derive(Serialize, Deserialize)]
struct WireMarker {
    schema: u64,
    owner: String,
    id: String,
}

fn marker_parse(text: &str) -> std::result::Result<Marker, String> {
    let wire = serde_json::from_str::<WireMarker>(text).map_err(|e| e.to_string())?;
    let owner = match wire.owner.as_str() {
        "process" => Owner::Process,
        "none" => Owner::None,
        other => return Err(format!("unknown owner `{other}`")),
    };
    Ok(Marker {
        owner,
        id: wire.id.parse::<InstanceId>().map_err(|e| e.to_string())?,
    })
}

pub(crate) fn read_marker(dir: &Path) -> Result<MarkerRead> {
    Ok(match read_file(dir, MARKER_FILE, marker_parse)? {
        Ok(Some(marker)) => MarkerRead::Valid(marker),
        Ok(None) => MarkerRead::Absent,
        Err(why) => MarkerRead::Corrupt(why),
    })
}

/// Replaces `owner.json` atomically.
pub(crate) fn write_marker(dir: &Path, marker: &Marker) -> Result<()> {
    let wire = WireMarker {
        schema: SCHEMA,
        owner: match marker.owner {
            Owner::Process => "process",
            Owner::None => "none",
        }
        .to_owned(),
        id: marker.id.to_string(),
    };
    let text = serde_json::to_string(&wire).map_err(|e| Error::new(ErrorKind::Internal, e.to_string()))?;
    write_file(dir, MARKER_FILE, &text)
}
