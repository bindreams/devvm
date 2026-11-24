//! `record.json` (what an instance is), `owner.json` (who keeps it alive) and `holder.json` (who holds its op lock): the
//! files an instance directory holds.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::SystemTime;

use cosca::identity::ProcessIdRecord;
use serde::{Deserialize, Serialize};

use crate::backend::MachineRecord;
use crate::contain::cosca_error;
use crate::host::{self, Host, remove_if_present, remove_tree};
use crate::instance::name::InstanceName;
use crate::random;
use crate::recipe::hash::LayerKey;
use crate::store::layers::{Generation, LayerRef};
use crate::{Arch, Error, ErrorKind, GuestKind, Network, Result};

#[cfg(test)]
#[path = "record_tests.rs"]
mod record_tests;

/// The schema of `record.json` and `owner.json` this devvm writes and reads.
pub(crate) const SCHEMA: u64 = 1;

pub(crate) const RECORD_FILE: &str = "record.json";
const MARKER_FILE: &str = "owner.json";

// Ids =================================================================================================================

/// The random identity of one instance, shown as 32 lowercase hex digits. A name can be reused; an id never is.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct InstanceId([u8; 16]);

const HOSTNAME_PREFIX: &str = "devvm-";
/// The hex digits of the id in a hostname: its first 4 bytes.
const HOSTNAME_BYTES: usize = 4;

/// The length of a hostname made by [`InstanceId::hostname`].
const HOSTNAME_LEN: usize = HOSTNAME_PREFIX.len() + 2 * HOSTNAME_BYTES;

// A Windows computer name has at most 15 characters.
const _: () = assert!(HOSTNAME_LEN <= 15);

impl InstanceId {
    pub(crate) fn random() -> InstanceId {
        InstanceId(random::bytes())
    }

    /// The name of the machine in its guest, Linux or Windows: `devvm-` and the first 8 hex digits of the id,
    /// [`HOSTNAME_LEN`] characters.
    pub(crate) fn hostname(&self) -> String {
        format!("{HOSTNAME_PREFIX}{}", hex::encode(&self.0[..HOSTNAME_BYTES]))
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

/// What [`write_file`] appends to a name for the file it writes first, followed by the hex of [`TEMP_BYTES`] bytes.
const TEMP_MARK: &str = ".tmp-";
const TEMP_BYTES: usize = 4;

/// Whether `name` is the temporary file of a [`write_file`] that was killed before its rename.
pub(crate) fn is_write_file_temporary(name: &str) -> bool {
    name.rsplit_once(TEMP_MARK)
        .is_some_and(|(base, digits)| !base.is_empty() && random::is_hex(digits, TEMP_BYTES))
}

/// Writes `text` to `dir/file` through a temp file and a rename, so a reader sees the old or the new content, never a
/// mixture, and a crash leaves no half-written `file`. `file` is the user's alone from the moment it exists, whatever
/// `dir` hands down: the temp file is private and a rename keeps its DACL or mode.
pub(crate) fn write_file(host: &dyn Host, dir: &Path, file: &str, text: &str) -> Result<()> {
    let suffix = random::bytes::<TEMP_BYTES>();
    let temp = dir.join(format!("{file}{TEMP_MARK}{}", hex::encode(suffix)));
    let target = dir.join(file);
    let fail = |action: &str, path: &Path, source| {
        Error::io(ErrorKind::Io, format!("cannot {action} {}", path.display()), source)
    };
    (|| {
        host.write_private(&temp, text.as_bytes())
            .map_err(|e| fail("write", &temp, e))?;
        host.replace_file(&temp, &target)
            .map_err(|e| fail("replace", &target, e))
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
            machine: self.machine.clone(),
        };
        let text = serde_json::to_string_pretty(&wire).map_err(|e| Error::new(ErrorKind::Internal, e.to_string()))?;
        // No caller of a record holds a host; every record is written by this process for its own user.
        write_file(host::current(), dir, RECORD_FILE, &text)
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

// The holder of the op lock ===========================================================================================

/// `holder.json`: the process that holds an instance's exclusive op lock, so a command that waits behind it can name it.
///
/// It is a record and not a question to the OS because std's file lock is `LockFileEx` on Windows, and no Windows call
/// names the holder of a lock. The holder writes it once it holds the lock and removes it before releasing the lock, so
/// a reader may see it written late or not at all, never half-written (see [`write_file`]); a reader names the process
/// only if it still runs (a crashed holder leaves its record behind).
///
/// A waiting command asks for the holder once, when its wait begins: the wait is one blocking OS call that does not wake
/// until the lock is free. The name it prints is the holder at that moment, so a holder that has taken the lock but
/// not yet written its record is not named, and a name stays on screen after its holder has gone.
///
/// Only the exclusive holder is recorded: a wait behind a shared holder, or behind the reaper, keeps the text without a
/// holder. A record whose removal failed and whose process still runs would be named by a wait behind such a holder; the
/// failed removal is a warning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Holder {
    pub process: ProcessIdRecord,
    /// The name of the process's executable: see [`program_name`].
    pub program: String,
}

pub(crate) const HOLDER_FILE: &str = "holder.json";

/// What a holder is called when its executable cannot be read.
const UNKNOWN_PROGRAM: &str = "unknown program";

#[derive(Serialize, Deserialize)]
struct WireHolder {
    schema: u64,
    process: ProcessIdRecord,
    program: String,
}

impl Holder {
    /// This process. The library can be embedded in any application, so only the program name is kept, never an
    /// argument.
    pub(crate) fn current() -> Result<Holder> {
        // `ProcessId::current` panics where the host refuses the process its own handle; `Process::current` is the
        // fallible call.
        let id = cosca::Process::current()
            .map_err(|e| cosca_error("cannot identify this process", e))?
            .id();
        let process = id
            .to_record()
            .map_err(|e| cosca_error(&format!("cannot record process {}", id.pid()), e))?;
        let program = std::env::current_exe()
            .ok()
            .and_then(|exe| program_name(&exe))
            .unwrap_or_else(|| UNKNOWN_PROGRAM.to_owned());
        Ok(Holder { process, program })
    }
}

/// The file name of `exe` without its directory and without a `.exe` extension (a name such as `devvm-0.1.0` or
/// `my.tool` keeps its dots); `None` if `exe` has no file name.
pub(crate) fn program_name(exe: &Path) -> Option<String> {
    let name = exe.file_name()?;
    let is_exe = exe
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"));
    let name = if is_exe { Path::new(name).file_stem()? } else { name };
    Some(name.to_string_lossy().into_owned())
}

fn holder_parse(text: &str) -> std::result::Result<Holder, String> {
    let wire = serde_json::from_str::<WireHolder>(text).map_err(|e| e.to_string())?;
    Ok(Holder {
        process: wire.process,
        program: wire.program,
    })
}

/// The holder recorded in `dir`: `None` when there is no record or it is unusable (torn, hand-edited), neither of which
/// names anyone. A record from a newer devvm is `Err(Store)`.
pub(crate) fn read_holder(dir: &Path) -> Result<Option<Holder>> {
    Ok(read_file(dir, HOLDER_FILE, holder_parse)?.unwrap_or(None))
}

/// Replaces `holder.json` atomically.
pub(crate) fn write_holder(dir: &Path, holder: &Holder) -> Result<()> {
    let wire = WireHolder {
        schema: SCHEMA,
        process: holder.process.clone(),
        program: holder.program.clone(),
    };
    let text = serde_json::to_string(&wire).map_err(|e| Error::new(ErrorKind::Internal, e.to_string()))?;
    write_file(host::current(), dir, HOLDER_FILE, &text)
}

/// Removes `holder.json`; one that is not there is as good as removed.
pub(crate) fn remove_holder(dir: &Path) -> Result<()> {
    let path = dir.join(HOLDER_FILE);
    remove_tree(&path).map_err(|e| Error::io(ErrorKind::Io, format!("cannot remove {}", path.display()), e))
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
    write_file(host::current(), dir, MARKER_FILE, &text)
}
