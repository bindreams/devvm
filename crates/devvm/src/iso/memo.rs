//! The hash of an ISO, remembered: a Windows ISO is gigabytes, and a layer key needs its SHA-256 on every build.
//!
//! One small file per ISO path (made absolute and lexically normal, so spellings of one path share it), in the cache
//! directory, holds the hash and what the ISO looked like when it was hashed: its size, modification time and file id.
//! The hash is reused while all of those, and the path, are unchanged; any one of them changing hashes the ISO again.
//! A memo is a cache: one that cannot be read is moved to quarantine and made again. The ISO itself is never copied or
//! kept; recipes keep it at their path.
//!
//! Writers hold the directory's `.lock` shared while they publish; whoever moves a memo to quarantine or sweeps the
//! temporary files a crashed writer left holds it exclusively, so neither can meet a live writer's file.

use std::fs::File;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256 as Sha2};

use crate::access::sftp::local::lexically_normal;
use crate::exit::Progress;
use crate::host::{FileId, Host, is_absent};
use crate::store::fetch::hash_reader;
use crate::store::layers::quarantine_file;
use crate::store::lock::{FileLock, Mode};
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
    /// `None` on a volume without file ids (exFAT, FAT32), where the other three stand alone.
    file_id: Option<FileId>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    path: String,
    identity: Identity,
    sha256: Sha256,
}

/// An ISO the memo knows, or a memo file that cannot be read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Known {
    Iso { path: PathBuf, sha256: Sha256 },
    Unreadable { file: PathBuf, why: String },
}

/// An ISO opened, and its SHA-256: the hash is of this file, not of whatever the path leads to later.
pub(crate) struct HashedIso {
    pub file: File,
    pub sha256: Sha256,
}

/// What a memo file turned out to be.
enum Slot {
    Absent,
    Entry(Entry),
    Junk,
}

/// What `quarantine_unless_valid` did with a memo file.
enum QuarantineOutcome {
    Valid(Entry),
    Moved(PathBuf),
    Gone,
}

/// The remembered hashes of ISOs.
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

    /// The SHA-256 of the ISO at `iso`; see [`IsoMemo::open`].
    pub(crate) fn sha256(&self, host: &dyn Host, iso: &Path, progress: &dyn Progress) -> Result<Sha256> {
        Ok(self.open(host, iso, progress)?.sha256)
    }

    /// Opens the ISO at `iso` and gives it with its SHA-256: the remembered one while the ISO is unchanged, else a
    /// fresh one, which is remembered. The ISO is read once and its identity is taken before and after; an ISO that
    /// changed while it was being hashed is an error, so a hash is never of two versions.
    pub(crate) fn open(&self, host: &dyn Host, iso: &Path, progress: &dyn Progress) -> Result<HashedIso> {
        let file = open(iso)?;
        let identity = identity_of(host, &file, iso)?;
        let named = named(iso)?;
        if let Some(text) = &named
            && let Some(entry) = self.read(text, progress)?
            && entry.identity == identity
        {
            return Ok(HashedIso {
                file,
                sha256: entry.sha256,
            });
        }
        let sha256 = hash_open(
            file.try_clone().map_err(|e| io(iso, "read", e))?,
            iso,
            identity.size,
            progress,
        )?;
        if identity_of(host, &file, iso)? != identity {
            return Err(Error::new(
                ErrorKind::Io,
                format!(
                    "{} changed while it was being hashed; run the command again",
                    iso.display()
                ),
            ));
        }
        if let Some(path) = named {
            self.write(host, &Entry { path, identity, sha256 }, progress)?;
        }
        Ok(HashedIso { file, sha256 })
    }

    /// The ISO at `iso` opened, when it is still the one `keyed` is the hash of: what a layer key was made from. Its
    /// hash comes from the memo while the ISO's identity matches it, and from reading the ISO otherwise (a touched,
    /// moved-back or memo-less ISO that holds the keyed bytes is still the keyed one). The caller reads through the
    /// file given, never through `iso` again.
    pub(crate) fn open_keyed(
        &self,
        host: &dyn Host,
        iso: &Path,
        keyed: Sha256,
        progress: &dyn Progress,
    ) -> Result<File> {
        let HashedIso { file, sha256 } = self.open(host, iso, progress)?;
        if sha256 == keyed {
            return Ok(file);
        }
        Err(Error::new(
            ErrorKind::Io,
            format!(
                "{} changed after the build was keyed by it; run the command again",
                iso.display()
            ),
        ))
    }

    /// The SHA-256 of the ISO at `iso` read now, whatever the memo says. Nothing is remembered.
    pub(crate) fn rehash(&self, iso: &Path, progress: &dyn Progress) -> Result<Sha256> {
        let file = open(iso)?;
        let size = file.metadata().map_err(|e| io(iso, "look at", e))?.len();
        hash_open(file, iso, size, progress)
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

    /// The memo file of the ISO at `text`, named by a hash of the path.
    fn file_of(&self, text: &str) -> PathBuf {
        let digest = Sha2::digest(text.as_bytes());
        self.dir.join(format!("{}.json", hex::encode(&digest[..16])))
    }

    fn lock(&self) -> Result<FileLock> {
        FileLock::open(&self.dir.join(".lock"))
    }

    /// The memo of the ISO at `text`, or `None` when it has none. A memo that cannot be read is moved to quarantine
    /// and counts as none.
    fn read(&self, text: &str, progress: &dyn Progress) -> Result<Option<Entry>> {
        let file = self.file_of(text);
        let accepts = |entry: &Entry| entry.path == text;
        match look(&file, &accepts)? {
            Slot::Absent => Ok(None),
            Slot::Entry(entry) => Ok(Some(entry)),
            Slot::Junk => Ok(match self.quarantine_unless_valid(&file, &accepts, progress)? {
                QuarantineOutcome::Valid(entry) => Some(entry),
                QuarantineOutcome::Moved(_) | QuarantineOutcome::Gone => None,
            }),
        }
    }

    /// Moves the memo `file` that cannot be read to quarantine, and says where it went; `None` when `file` is a memo
    /// after all, or is not there.
    pub(crate) fn quarantine_unreadable(&self, file: &Path, progress: &dyn Progress) -> Result<Option<PathBuf>> {
        Ok(match self.quarantine_unless_valid(file, &|_| true, progress)? {
            QuarantineOutcome::Moved(target) => Some(target),
            QuarantineOutcome::Valid(_) | QuarantineOutcome::Gone => None,
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
    ) -> Result<QuarantineOutcome> {
        let mut lock = self.lock()?;
        lock.lock(Mode::Exclusive, "another devvm process writing an ISO memo", &mut |m| {
            progress.message(m)
        })?;
        Ok(match look(file, accepts)? {
            Slot::Absent => QuarantineOutcome::Gone,
            Slot::Entry(entry) => QuarantineOutcome::Valid(entry),
            Slot::Junk => match quarantine_file(&self.quarantine, "iso-memo", file, progress)? {
                Some(target) => QuarantineOutcome::Moved(target),
                None => QuarantineOutcome::Gone,
            },
        })
    }

    /// Publishes `entry` by rename, so a reader sees the whole file or none and two writers of one ISO cannot tear it.
    /// The temporary files a writer that died left are removed first, when no writer is live.
    fn write(&self, host: &dyn Host, entry: &Entry, progress: &dyn Progress) -> Result<()> {
        let mut lock = self.lock()?;
        if lock.try_lock(Mode::Exclusive)? {
            self.sweep_leftovers(progress);
            lock.unlock()?;
        }
        lock.lock(Mode::Shared, "another devvm process clearing ISO memos", &mut |m| {
            progress.message(m)
        })?;
        let file = self.file_of(&entry.path);
        let temp = file.with_extension(format!("{}.tmp", hex::encode(random::bytes::<8>())));
        let bytes = serde_json::to_vec_pretty(entry).map_err(|e| Error::new(ErrorKind::Internal, e.to_string()))?;
        std::fs::write(&temp, bytes)
            .and_then(|()| host.replace_file(&temp, &file))
            .map_err(|e| {
                let cleanup = crate::host::remove_if_present(&temp);
                Error::write(&file, e, "free some space and run again")
                    .after_cleanup(&temp.display().to_string(), cleanup)
            })
    }

    /// Removes the `*.tmp` files of `dir`: what a writer that died between making its file and publishing it left. The
    /// caller holds the lock exclusively, and a live writer holds it shared, so every such file is dead.
    fn sweep_leftovers(&self, progress: &dyn Progress) {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) => {
                progress.message(&format!("cannot list {} for leftovers: {e}", self.dir.display()));
                return;
            }
        };
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(e) => {
                    progress.message(&format!("cannot list an entry of {}: {e}", self.dir.display()));
                    continue;
                }
            };
            if path.extension().is_some_and(|extension| extension == "tmp")
                && let Err(e) = crate::host::remove_if_present(&path)
            {
                progress.message(&format!("cannot remove leftover {}: {e}", path.display()));
            }
        }
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

/// `iso` as a memo names it: absolute and lexically normal, so one path in different spellings has one memo. `None`
/// when it is not Unicode, which a memo cannot name.
fn named(iso: &Path) -> Result<Option<String>> {
    let absolute = std::path::absolute(iso).map_err(|e| io(iso, "resolve", e))?;
    Ok(lexically_normal(&absolute).to_str().map(str::to_owned))
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

/// The ISO opened for reading; a missing one is `NotFound`.
fn open(iso: &Path) -> Result<File> {
    File::open(iso).map_err(|e| {
        if is_absent(&e) {
            Error::new(ErrorKind::NotFound, format!("the ISO {} does not exist", iso.display())).with_source(e)
        } else {
            io(iso, "open", e)
        }
    })
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

fn hash_open(file: File, iso: &Path, size: u64, progress: &dyn Progress) -> Result<Sha256> {
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
