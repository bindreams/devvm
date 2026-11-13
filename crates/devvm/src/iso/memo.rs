//! The hash of an ISO, remembered: a Windows ISO is gigabytes, and a layer key needs its SHA-256 on every build.
//!
//! One small file per ISO path, in the cache directory, holds the hash and what the ISO looked like when it was hashed:
//! its size, modification time and file id. The hash is reused while all of those, and the path, are unchanged; any one
//! of them changing hashes the ISO again. A memo is a cache: one that cannot be read is moved to quarantine and made
//! again. The ISO itself is never copied or kept; recipes keep it at their path.

use std::fs::File;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256 as Sha2};

use crate::exit::Progress;
use crate::host::{FileId, Host, is_absent};
use crate::store::fetch::hash_reader;
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

    /// The SHA-256 of the ISO at `iso`: the remembered one while the ISO is unchanged, else a fresh one, which is
    /// remembered. The ISO is read once and its identity is taken before and after; an ISO that changed while it was
    /// being hashed is an error, so a hash is never of two versions.
    pub(crate) fn sha256(&self, host: &dyn Host, iso: &Path, progress: &dyn Progress) -> Result<Sha256> {
        let file = open(iso)?;
        let identity = identity_of(host, &file, iso)?;
        if let Some(entry) = self.read(iso, progress)?
            && entry.identity == identity
        {
            return Ok(entry.sha256);
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
        if let Some(text) = iso.to_str() {
            let entry = Entry {
                path: text.to_owned(),
                identity,
                sha256,
            };
            self.write(host, &entry)?;
        }
        Ok(sha256)
    }

    /// Succeeds when the ISO at `iso` is still the one `keyed` is the hash of: what a layer key was made from. A path
    /// that is not Unicode has no memo (a memo names its ISO as text), so the ISO is hashed again.
    pub(crate) fn check_unchanged(
        &self,
        host: &dyn Host,
        iso: &Path,
        keyed: Sha256,
        progress: &dyn Progress,
    ) -> Result<()> {
        let file = open(iso)?;
        let unchanged = match iso.to_str() {
            Some(_) => {
                let identity = identity_of(host, &file, iso)?;
                self.read(iso, progress)?
                    .is_some_and(|entry| entry.sha256 == keyed && entry.identity == identity)
            }
            None => self.rehash(iso, progress)? == keyed,
        };
        if unchanged {
            return Ok(());
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

    /// The memo of `iso`, or `None` when it has none. A memo that cannot be read is moved to quarantine and counts as
    /// none.
    fn read(&self, iso: &Path, progress: &dyn Progress) -> Result<Option<Entry>> {
        let Some(text) = iso.to_str() else { return Ok(None) };
        let file = self.file_of(text);
        match std::fs::read(&file) {
            Err(e) if is_absent(&e) => return Ok(None),
            Err(e) => return Err(io(&file, "read", e)),
            Ok(bytes) => {
                if let Ok(entry) = serde_json::from_slice::<Entry>(&bytes)
                    && entry.path == text
                {
                    return Ok(Some(entry));
                }
            }
        }
        self.quarantine(&file, progress)?;
        Ok(None)
    }

    fn quarantine(&self, file: &Path, progress: &dyn Progress) -> Result<()> {
        std::fs::create_dir_all(&self.quarantine).map_err(|e| io(&self.quarantine, "make", e))?;
        let target = self
            .quarantine
            .join(format!("iso-memo-{}.json", hex::encode(random::bytes::<8>())));
        std::fs::rename(file, &target).map_err(|e| io(file, "move to quarantine", e))?;
        progress.message(&format!(
            "moved the unreadable {} to {}",
            file.display(),
            target.display()
        ));
        Ok(())
    }

    /// Publishes `entry` by rename, so a reader sees the whole file or none and two writers of one ISO cannot tear it.
    fn write(&self, host: &dyn Host, entry: &Entry) -> Result<()> {
        std::fs::create_dir_all(&self.dir).map_err(|e| io(&self.dir, "make", e))?;
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
