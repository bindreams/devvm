//! The hash and image list of an ISO, remembered: a Windows ISO is gigabytes, and a layer key needs its SHA-256 and its
//! image list on every build.
//!
//! One small file per ISO, in the cache directory, holds the hash, the image list and what the ISO looked like when
//! they were read: its size, modification time and file id. The ISO is named by its file id (volume and file number),
//! so every spelling of one file shares one memo: another case, a junction, a symlink, a short name. A volume without
//! file ids (exFAT, FAT32) names it by its canonical path instead, which resolves links and case, and tells two ISOs
//! apart by size and time alone: a file replaced in place by one of the same size and time there is taken for the old
//! one. The hash and the list are reused while the ISO's identity is unchanged; any change reads the ISO again. A memo
//! is a cache: one that cannot be read is moved to quarantine and made again. The ISO itself is never copied or kept;
//! recipes keep it at their path.
//!
//! Writers hold the directory's `.lock` shared while they publish; whoever moves a memo to quarantine or sweeps the
//! temporary files a crashed writer left holds it exclusively, so neither can meet a live writer's file.

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256 as Sha2};

use super::{HeldIso, WimImage};
use crate::exit::Progress;
use crate::host::{FileId, Host, is_absent, remove_if_present, sweep};
use crate::store::fetch::hash_reader;
use crate::store::lock::{FileLock, Mode};
use crate::store::quarantine::quarantine_file;
use crate::{Dirs, Error, ErrorKind, Result, Sha256, random};

#[cfg(test)]
#[path = "memo_tests.rs"]
mod memo_tests;

/// Progress is reported each time the byte count crosses a multiple of this.
const PROGRESS_STEP: u64 = 256 << 20;

/// What an ISO looked like when it was hashed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    size: u64,
    mtime_seconds: i64,
    mtime_nanos: i32,
    /// `None` on a volume without file ids (exFAT, FAT32), where the other two stand alone.
    file_id: Option<FileId>,
}

/// How a memo names its ISO.
struct Names {
    /// What the memo file's name is made from: the file id, or the canonical path where there is none.
    key: String,
    /// The ISO's absolute path as it was given, for `devvm doctor`.
    path: String,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    key: String,
    path: String,
    identity: Identity,
    sha256: Sha256,
    images: Vec<WimImage>,
}

/// An ISO the memo knows, or a memo file that cannot be read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Known {
    Iso { path: PathBuf, sha256: Sha256 },
    Unreadable { file: PathBuf, why: String },
}

/// An ISO held, with its SHA-256 and its image list: both are of this file, not of whatever the path leads to later.
#[derive(Debug)]
pub(crate) struct HashedIso {
    pub iso: HeldIso,
    pub sha256: Sha256,
    pub images: Vec<WimImage>,
}

/// What a memo file turned out to be.
enum Slot {
    Absent,
    Entry(Entry),
    Junk,
}

/// What `quarantine_unless_valid` did with a memo file.
enum Handled {
    Valid(Entry),
    Moved(PathBuf),
    Gone,
}

/// What [`IsoMemo::quarantine_unreadable`] did with a memo file.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum QuarantineOutcome {
    /// It is a memo after all, of this ISO.
    Valid { path: PathBuf, sha256: Sha256 },
    /// It was moved here.
    Moved(PathBuf),
    /// It is not there: another process handled it.
    Gone,
}

/// The remembered hashes and image lists of ISOs.
pub(crate) struct IsoMemo {
    dir: PathBuf,
    quarantine: PathBuf,
}

impl IsoMemo {
    pub(crate) fn new(dirs: &Dirs) -> IsoMemo {
        IsoMemo {
            dir: dirs.iso_memo(),
            quarantine: dirs.quarantine(),
        }
    }

    /// Opens and holds the ISO at `iso` ([`HeldIso::open`]) and gives it with its SHA-256 and image list: the
    /// remembered ones while the ISO is unchanged, else fresh ones, which are remembered. `read_images` reads the list
    /// from the ISO, before the hash, so a file that is no installer is refused without hashing it. The ISO cannot
    /// change while it is held, so the hash and the list are of one version.
    pub(crate) fn open(
        &self,
        host: &dyn Host,
        iso: &Path,
        progress: &dyn Progress,
        read_images: &dyn Fn(&HeldIso) -> Result<Vec<WimImage>>,
    ) -> Result<HashedIso> {
        let held = HeldIso::open(host, iso)?;
        let identity = identity_of(host, held.file(), iso)?;
        let names = names_of(&identity, iso)?;
        if let Some(names) = &names
            && let Some(entry) = self.read(&names.key, progress)?
            && entry.identity == identity
        {
            return Ok(HashedIso {
                iso: held,
                sha256: entry.sha256,
                images: entry.images,
            });
        }
        let images = read_images(&held)?;
        let sha256 = hash_held(&held, identity.size, progress)?;
        if let Some(Names { key, path }) = names {
            let entry = Entry {
                key,
                path,
                identity,
                sha256,
                images: images.clone(),
            };
            self.write(host, &entry, progress)?;
        }
        Ok(HashedIso {
            iso: held,
            sha256,
            images,
        })
    }

    /// The SHA-256 of the ISO at `iso` read now, whatever the memo says. Nothing is remembered.
    pub(crate) fn rehash(&self, host: &dyn Host, iso: &Path, progress: &dyn Progress) -> Result<Sha256> {
        let held = HeldIso::open(host, iso)?;
        let size = held.file().metadata().map_err(|e| io(iso, "look at", e))?.len();
        hash_held(&held, size, progress)
    }

    /// Every ISO the memo knows, by path.
    pub(crate) fn known(&self) -> Result<Vec<Known>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if is_absent(&e) => return Ok(Vec::new()),
            Err(e) => return Err(io(&self.dir, "list", e)),
        };
        let mut known = Vec::new();
        for entry in entries {
            let file = entry.map_err(|e| io(&self.dir, "list", e))?.path();
            if file.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            known.push(match read_entry_file(&file) {
                Ok(entry) => Known::Iso {
                    path: PathBuf::from(entry.path),
                    sha256: entry.sha256,
                },
                Err(why) => Known::Unreadable { file, why },
            });
        }
        known.sort_by(|a, b| sort_key(a).cmp(sort_key(b)));
        Ok(known)
    }

    /// The memo file of the ISO named by `key`, named by a hash of the key.
    fn file_of(&self, key: &str) -> PathBuf {
        let digest = Sha2::digest(key.as_bytes());
        self.dir.join(format!("{}.json", hex::encode(&digest[..16])))
    }

    fn lock(&self) -> Result<FileLock> {
        FileLock::open(&self.dir.join(".lock"))
    }

    /// The memo of the ISO named by `key`, or `None` when it has none. A memo that cannot be read is moved to
    /// quarantine and counts as none.
    fn read(&self, key: &str, progress: &dyn Progress) -> Result<Option<Entry>> {
        let file = self.file_of(key);
        let accepts = |entry: &Entry| entry.key == key;
        match look(&file, &accepts)? {
            Slot::Absent => Ok(None),
            Slot::Entry(entry) => Ok(Some(entry)),
            Slot::Junk => Ok(match self.quarantine_unless_valid(&file, &accepts, progress)? {
                Handled::Valid(entry) => Some(entry),
                Handled::Moved(_) | Handled::Gone => None,
            }),
        }
    }

    /// Moves the memo `file` that cannot be read to quarantine, and says what became of it.
    pub(crate) fn quarantine_unreadable(&self, file: &Path, progress: &dyn Progress) -> Result<QuarantineOutcome> {
        Ok(match self.quarantine_unless_valid(file, &|_| true, progress)? {
            Handled::Valid(entry) => QuarantineOutcome::Valid {
                path: PathBuf::from(entry.path),
                sha256: entry.sha256,
            },
            Handled::Moved(target) => QuarantineOutcome::Moved(target),
            Handled::Gone => QuarantineOutcome::Gone,
        })
    }

    /// Moves `file` to quarantine unless it is a memo `accepts`. The look that decides is taken with the directory's
    /// lock held exclusively, which no writer holds while it publishes: a memo a writer has just made is never moved,
    /// and one another process already moved is `Gone`.
    fn quarantine_unless_valid(
        &self,
        file: &Path,
        accepts: &dyn Fn(&Entry) -> bool,
        progress: &dyn Progress,
    ) -> Result<Handled> {
        let mut lock = self.lock()?;
        lock.lock(Mode::Exclusive, "another devvm process writing an ISO memo", &mut |m| {
            progress.message(m)
        })?;
        Ok(match look(file, accepts)? {
            Slot::Absent => Handled::Gone,
            Slot::Entry(entry) => Handled::Valid(entry),
            Slot::Junk => match quarantine_file(&self.quarantine, "iso-memo", file, progress)? {
                Some(target) => Handled::Moved(target),
                None => Handled::Gone,
            },
        })
    }

    /// Publishes `entry` by rename, so a reader sees the whole file or none and two writers of one ISO cannot tear it.
    /// The temporary files a writer that died left are removed first, when no writer is live.
    fn write(&self, host: &dyn Host, entry: &Entry, progress: &dyn Progress) -> Result<()> {
        let mut lock = self.lock()?;
        if lock.try_lock(Mode::Exclusive)? {
            match sweep(&self.dir, &|name| name.ends_with(".tmp"), &remove_if_present) {
                Ok(warnings) => warnings.iter().for_each(|warning| progress.message(warning)),
                Err(e) => progress.message(e.message()),
            }
            lock.unlock()?;
        }
        lock.lock(Mode::Shared, "another devvm process clearing ISO memos", &mut |m| {
            progress.message(m)
        })?;
        let file = self.file_of(&entry.key);
        let temp = file.with_extension(format!("{}.tmp", hex::encode(random::bytes::<8>())));
        let bytes = serde_json::to_vec_pretty(entry).map_err(|e| Error::new(ErrorKind::Internal, e.to_string()))?;
        std::fs::write(&temp, bytes)
            .and_then(|()| host.replace_file(&temp, &file))
            .map_err(|e| {
                let cleanup = remove_if_present(&temp);
                Error::write(&file, e, "free some space and run again")
                    .after_cleanup(&temp.display().to_string(), cleanup)
            })
    }
}

/// What the memo file `file` holds: nothing, an entry `accepts`, or something else.
fn look(file: &Path, accepts: &dyn Fn(&Entry) -> bool) -> Result<Slot> {
    Ok(match std::fs::read(file) {
        Err(e) if is_absent(&e) => Slot::Absent,
        Err(e) => return Err(io(file, "read", e)),
        Ok(bytes) => match serde_json::from_slice::<Entry>(&bytes) {
            Ok(entry) if accepts(&entry) => Slot::Entry(entry),
            _ => Slot::Junk,
        },
    })
}

/// How a memo names the ISO at `iso`, whose identity is `identity`: by file id, or by canonical path where the file
/// system has none. `None` when a name is not Unicode, which a memo cannot hold: that ISO is not remembered.
fn names_of(identity: &Identity, iso: &Path) -> Result<Option<Names>> {
    let absolute = std::path::absolute(iso).map_err(|e| io(iso, "resolve", e))?;
    let Some(path) = absolute.to_str() else {
        return Ok(None);
    };
    let key = match identity.file_id {
        Some(id) => format!("id:{:x}:{:x}", id.volume, id.file),
        None => {
            let canonical = std::fs::canonicalize(iso).map_err(|e| io(iso, "resolve", e))?;
            match canonical.to_str() {
                Some(text) => format!("path:{text}"),
                None => return Ok(None),
            }
        }
    };
    Ok(Some(Names {
        key,
        path: path.to_owned(),
    }))
}

fn sort_key(known: &Known) -> &Path {
    match known {
        Known::Iso { path, .. } => path,
        Known::Unreadable { file, .. } => file,
    }
}

fn read_entry_file(file: &Path) -> std::result::Result<Entry, String> {
    let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| format!("it is not a memo: {e}"))
}

fn io(path: &Path, what: &str, error: std::io::Error) -> Error {
    Error::io(ErrorKind::Io, format!("cannot {what} {}", path.display()), error)
}

fn identity_of(host: &dyn Host, file: &File, iso: &Path) -> Result<Identity> {
    let meta = file.metadata().map_err(|e| io(iso, "look at", e))?;
    let modified = meta
        .modified()
        .map_err(|e| io(iso, "read the modification time of", e))?;
    let modified = jiff::Timestamp::try_from(modified).map_err(|e| {
        Error::new(
            ErrorKind::Io,
            format!("the modification time of {} is out of range: {e}", iso.display()),
        )
    })?;
    let file_id = match host.file_id_of_open(file) {
        Ok(id) => Some(id),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
            ) =>
        {
            None
        }
        Err(e) => return Err(io(iso, "read the file id of", e)),
    };
    Ok(Identity {
        size: meta.len(),
        mtime_seconds: modified.as_second(),
        mtime_nanos: modified.subsec_nanosecond(),
        file_id,
    })
}

/// The SHA-256 of all of `held`, from its start; `size` is for the progress lines.
fn hash_held(held: &HeldIso, size: u64, progress: &dyn Progress) -> Result<Sha256> {
    let iso = held.path();
    let mut file = held.file();
    file.seek(SeekFrom::Start(0)).map_err(|e| io(iso, "read", e))?;
    progress.message(&format!("hashing {}: 0 of {size} bytes", iso.display()));
    let mut reported = 0;
    let hasher = hash_reader(file, iso, &mut |done| {
        if done / PROGRESS_STEP > reported / PROGRESS_STEP {
            progress.message(&format!("hashing {}: {done} of {size} bytes", iso.display()));
        }
        reported = done;
    })?;
    Ok(Sha256::from_bytes(hasher.finalize().into()))
}
