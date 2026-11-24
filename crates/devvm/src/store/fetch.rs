//! Verified, resumable downloads, cached by content hash.
//!
//! On disk, under the downloads directory:
//!
//! - `<hash>/<file-name>`: a verified artifact, `<hash>` being the first 64 hex digits of the pinned digest (all of a
//!   sha256, half of a sha512, so a path stays short enough for `qemu-img`). It exists only after verification and is
//!   published by rename; a sha512 entry is checked against the whole digest on every later use;
//! - `<hash>.part` and `<hash>.part.json` (url and validator): a download in progress, kept across interruptions;
//! - `<hash>.lock`: one OS lock per artifact, held for the whole fetch.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256 as Sha2, Sha512 as Sha2_512};

use crate::error::text_with_sources;
use crate::progress::{Progress, Total, human_bytes};
use crate::store::lock::{FileLock, Mode};
use crate::{ContentHash, Error, ErrorKind, Result, Sha256, Sha512};

/// A full disk keeps the part file, so the next run continues from it.
const FULL_DISK_HINT: &str = "free some space and run again to resume";

mod http;
mod name;

pub use http::{HttpTransport, LazyHttp};

#[cfg(test)]
#[path = "fetch_tests.rs"]
pub(crate) mod fetch_tests;

#[cfg(test)]
#[path = "fetch/http_tests.rs"]
pub(crate) mod http_tests;

// Transport ===========================================================================================================

/// A file pinned by content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub url: String,
    pub hash: ContentHash,
}

/// How many hex digits of the pinned digest name an artifact's files.
const KEY_DIGITS: usize = 64;

impl Artifact {
    /// The name its files carry in the downloads directory: the first [`KEY_DIGITS`] hex digits of its digest.
    pub(crate) fn cache_key(&self) -> String {
        let mut hex = self.hash.to_string();
        hex.truncate(KEY_DIGITS);
        hex
    }
}

/// Where to continue an interrupted download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resume {
    pub from: u64,
    /// The strong `ETag`, else `Last-Modified`, the first attempt saw; sent as `If-Range`.
    pub validator: Option<String>,
}

/// A `Content-Range` answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentRange {
    /// From a 206: `bytes <start>-<end>/<total>`; `total` is `None` for `*`.
    Range { start: u64, end: u64, total: Option<u64> },
    /// From a 416: `bytes */<total>`.
    Unsatisfied { total: u64 },
}

/// The start of an HTTP answer.
pub struct Response {
    pub status: u16,
    pub content_range: Option<ContentRange>,
    /// The body length the server declared, if it did.
    pub content_length: Option<u64>,
    /// A strong `ETag`, else `Last-Modified`; `None` when the server gave neither.
    pub validator: Option<String>,
    /// Headers the transport had to ignore, one sentence each; the fetcher reports them.
    pub warnings: Vec<String>,
    pub body: Box<dyn Read + Send>,
}

/// Fetches bytes. Implementations return `Err(Download)` naming the URL and never panic.
pub trait Transport: Send + Sync {
    fn get(&self, url: &str, resume: Option<Resume>) -> Result<Response>;
}

// Part file seam ======================================================================================================

/// The part file's writer. Production writes a `File`; tests inject faults such as a full disk.
pub(crate) trait PartWriter: Send {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    /// Flushes everything to disk.
    fn sync(&mut self) -> std::io::Result<()>;
}

pub(crate) trait PartOpener: Send + Sync {
    /// Opens the part file for appending (`append`) or recreates it empty.
    fn open(&self, path: &Path, append: bool) -> std::io::Result<Box<dyn PartWriter>>;
}

struct FileWriter(File);

impl PartWriter for FileWriter {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.0.write_all(bytes)
    }

    fn sync(&mut self) -> std::io::Result<()> {
        self.0.sync_all()
    }
}

struct FileOpener;

impl PartOpener for FileOpener {
    fn open(&self, path: &Path, append: bool) -> std::io::Result<Box<dyn PartWriter>> {
        let mut options = std::fs::OpenOptions::new();
        if append {
            options.append(true).create(true);
        } else {
            options.write(true).create(true).truncate(true);
        }
        Ok(Box::new(FileWriter(options.open(path)?)))
    }
}

// Fetcher =============================================================================================================

#[derive(Serialize, Deserialize)]
struct Sidecar {
    url: String,
    validator: Option<String>,
}

pub struct Fetcher {
    downloads: PathBuf,
    transport: Arc<dyn Transport>,
    progress: Arc<dyn Progress>,
    opener: Arc<dyn PartOpener>,
}

/// How an attempt will proceed.
enum Plan {
    /// The part is already the whole artifact (the server answered 416 for its length).
    Complete,
    /// Continue the part with `response`, which starts at byte `from`.
    Append { response: Response, from: u64 },
    /// Start over with `response`.
    Fresh { response: Response },
}

impl Fetcher {
    pub fn new(downloads_dir: PathBuf, transport: Arc<dyn Transport>, progress: Arc<dyn Progress>) -> Fetcher {
        Fetcher::with_part_opener(downloads_dir, transport, progress, Arc::new(FileOpener))
    }

    pub(crate) fn with_part_opener(
        downloads_dir: PathBuf,
        transport: Arc<dyn Transport>,
        progress: Arc<dyn Progress>,
        opener: Arc<dyn PartOpener>,
    ) -> Fetcher {
        Fetcher {
            downloads: downloads_dir,
            transport,
            progress,
            opener,
        }
    }

    /// Where the verified file for `artifact` lives once fetched: `<downloads>/<hash>/<file-name>`.
    pub fn entry_path(&self, artifact: &Artifact) -> PathBuf {
        self.downloads
            .join(artifact.cache_key())
            .join(name::file_name(&artifact.url))
    }

    fn part_path(&self, artifact: &Artifact) -> PathBuf {
        self.downloads.join(format!("{}.part", artifact.cache_key()))
    }

    fn sidecar_path(&self, artifact: &Artifact) -> PathBuf {
        self.downloads.join(format!("{}.part.json", artifact.cache_key()))
    }

    /// Returns the verified file, downloading it first if needed.
    ///
    /// One fetch per artifact runs at a time across processes; a second one waits, then finds the file. An
    /// interrupted download resumes from the part file. A complete download whose hash is wrong is deleted
    /// (`Checksum`).
    pub fn fetch(&self, artifact: &Artifact) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.downloads)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", self.downloads.display()), e))?;
        let mut lock = FileLock::open(&self.downloads.join(format!("{}.lock", artifact.cache_key())))?;
        lock.lock(
            Mode::Exclusive,
            &format!("another devvm process to finish downloading {}", artifact.url),
            &mut |m| self.progress.message(m),
        )?;
        let entry = self.entry_path(artifact);
        if entry.is_file() {
            self.check_entry(artifact, &entry)?;
            // A crash between publishing and the cleanup in `verify_and_publish` leaves these behind.
            remove_if_present(&self.sidecar_path(artifact))?;
            remove_if_present(&self.sidecar_temp_path(artifact))?;
            return Ok(entry);
        }
        self.download(artifact, &entry)?;
        Ok(entry)
    }

    /// A key that holds the whole digest names the entry's content; a shorter one is a prefix, so the entry is hashed
    /// against the pin.
    fn check_entry(&self, artifact: &Artifact, entry: &Path) -> Result<()> {
        if artifact.hash.to_string().len() == KEY_DIGITS {
            return Ok(());
        }
        let actual = Hasher::of_file(&artifact.hash, entry)?.finish();
        if actual != artifact.hash {
            return Err(Error::new(
                ErrorKind::Checksum,
                format!(
                    "{} has {} {actual}, expected {}",
                    entry.display(),
                    artifact.hash.algorithm(),
                    artifact.hash
                ),
            ));
        }
        Ok(())
    }

    // Download --------------------------------------------------------------------------------------------------------

    fn download(&self, artifact: &Artifact, entry: &Path) -> Result<()> {
        let part = self.part_path(artifact);
        let (plan, total) = match self.plan(artifact, &part)? {
            Plan::Complete => {
                let have = file_len(&part)?;
                return self.verify_and_publish(
                    artifact,
                    &part,
                    entry,
                    Hasher::of_file(&artifact.hash, &part)?.finish(),
                    have,
                );
            }
            Plan::Append { response, from } => {
                let total = total_size(&response, from);
                (Plan::Append { response, from }, total)
            }
            Plan::Fresh { response } => {
                let total = total_size(&response, 0);
                (Plan::Fresh { response }, total)
            }
        };

        let (mut response, from, mut hasher, fresh) = match plan {
            Plan::Append { response, from } => (response, from, Hasher::of_file(&artifact.hash, &part)?, false),
            Plan::Fresh { response } => (response, 0, Hasher::new(&artifact.hash), true),
            Plan::Complete => unreachable!("handled above"),
        };

        let mut writer = self
            .opener
            .open(&part, from > 0)
            .map_err(|e| Error::write(&part, e, FULL_DISK_HINT))?;
        if fresh {
            // After the truncation: a failed open must leave the old validator beside the old part.
            self.write_sidecar(artifact, response.validator.clone())?;
        }
        self.progress.debug(&format!("downloading {}", artifact.url));
        let task = self.progress.task(
            format!("downloading {}", name::file_name(&artifact.url)),
            total.map_or(Total::Unknown, Total::Bytes),
        );
        if from > 0 {
            task.status(format!("resuming at {}", human_bytes(from)));
            task.set(from);
        }

        let mut buffer = vec![0u8; 64 * 1024];
        let mut done = from;
        loop {
            let read = match response.body.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(Error::new(
                        ErrorKind::Download,
                        format!(
                            "download of {} stopped after {done} bytes: {}; the partial file is kept for a retry",
                            artifact.url,
                            text_with_sources(&e)
                        ),
                    )
                    .with_source(e));
                }
            };
            writer
                .write_all(&buffer[..read])
                .map_err(|e| Error::write(&part, e, FULL_DISK_HINT))?;
            hasher.update(&buffer[..read]);
            done += read as u64;
            task.set(done);
        }
        writer.sync().map_err(|e| Error::write(&part, e, FULL_DISK_HINT))?;
        drop(writer);

        if let Some(length) = response.content_length
            && done - from != length
        {
            return Err(Error::new(
                ErrorKind::Download,
                format!(
                    "download of {} ended after {} of {length} bytes; the partial file is kept for a retry",
                    artifact.url,
                    done - from
                ),
            ));
        }
        self.verify_and_publish(artifact, &part, entry, hasher.finish(), done)?;
        task.finish();
        Ok(())
    }

    /// Decides where to start: a resume when a part from the same URL exists, else from zero.
    fn plan(&self, artifact: &Artifact, part: &Path) -> Result<Plan> {
        let have = match file_len_opt(part)? {
            Some(have) if have > 0 => have,
            _ => return self.fresh(artifact, part),
        };
        let sidecar = match self.read_sidecar(artifact) {
            Ok(sidecar) if sidecar.url == artifact.url => sidecar,
            Ok(sidecar) => return self.restart(artifact, part, &format!("its partial file is from {}", sidecar.url)),
            Err(why) => return self.restart(artifact, part, &why),
        };
        let response = self.get(
            &artifact.url,
            Some(Resume {
                from: have,
                validator: sidecar.validator,
            }),
        )?;
        match (response.status, response.content_range) {
            (200, _) => {
                self.report_abandoned(
                    artifact,
                    &format!("the server sent the whole file instead of continuing at {have} bytes"),
                );
                Ok(Plan::Fresh { response })
            }
            (206, Some(ContentRange::Range { start, .. })) if start == have => {
                Ok(Plan::Append { response, from: have })
            }
            (416, Some(ContentRange::Unsatisfied { total })) if total == have => Ok(Plan::Complete),
            (status @ (206 | 416), content_range) => {
                let said = match content_range {
                    Some(ContentRange::Range { start, .. }) => format!("a range starting at byte {start}"),
                    Some(ContentRange::Unsatisfied { total }) => format!("a total of {total} bytes"),
                    None => "no usable Content-Range".to_owned(),
                };
                self.restart(
                    artifact,
                    part,
                    &format!(
                        "the server answered {status} with {said}, not what {have} bytes of partial file call for"
                    ),
                )
            }
            (status, _) => Err(unexpected_status(&artifact.url, status)),
        }
    }

    /// The sidecar of the part in progress; `Err` says why it cannot be used.
    fn read_sidecar(&self, artifact: &Artifact) -> std::result::Result<Sidecar, String> {
        let path = self.sidecar_path(artifact);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{} is unreadable: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err("there is no record of where its partial file came from".to_owned())
            }
            Err(e) => Err(format!("{} is unreadable: {e}", path.display())),
        }
    }

    /// Gives up the resume for `why`, says so, and downloads from the start.
    fn restart(&self, artifact: &Artifact, part: &Path, why: &str) -> Result<Plan> {
        self.report_abandoned(artifact, why);
        self.fresh(artifact, part)
    }

    fn report_abandoned(&self, artifact: &Artifact, why: &str) {
        self.progress.warn(&format!(
            "not resuming {}: {why}; downloading it from the start",
            artifact.url
        ));
    }

    /// `Transport::get`, passing the transport's header warnings on to the progress sink.
    fn get(&self, url: &str, resume: Option<Resume>) -> Result<Response> {
        let response = self.transport.get(url, resume)?;
        for warning in &response.warnings {
            self.progress.warn(&format!("{url}: {warning}"));
        }
        Ok(response)
    }

    /// Discards any part and asks for the whole file.
    fn fresh(&self, artifact: &Artifact, part: &Path) -> Result<Plan> {
        remove_if_present(part)?;
        remove_if_present(&self.sidecar_path(artifact))?;
        let response = self.get(&artifact.url, None)?;
        match response.status {
            200 => Ok(Plan::Fresh { response }),
            status => Err(unexpected_status(&artifact.url, status)),
        }
    }

    fn sidecar_temp_path(&self, artifact: &Artifact) -> PathBuf {
        self.downloads.join(format!("{}.part.json.tmp", artifact.cache_key()))
    }

    fn write_sidecar(&self, artifact: &Artifact, validator: Option<String>) -> Result<()> {
        let sidecar = Sidecar {
            url: artifact.url.clone(),
            validator,
        };
        let path = self.sidecar_path(artifact);
        let temp = self.sidecar_temp_path(artifact);
        let json = serde_json::to_vec(&sidecar).map_err(|e| Error::new(ErrorKind::Internal, e.to_string()))?;
        std::fs::write(&temp, json)
            .and_then(|()| std::fs::rename(&temp, &path))
            .map_err(|e| Error::write(&path, e, FULL_DISK_HINT))
    }

    fn verify_and_publish(
        &self,
        artifact: &Artifact,
        part: &Path,
        entry: &Path,
        actual: ContentHash,
        size: u64,
    ) -> Result<()> {
        if actual != artifact.hash {
            remove_if_present(part)?;
            remove_if_present(&self.sidecar_path(artifact))?;
            return Err(Error::new(
                ErrorKind::Checksum,
                format!(
                    "{} ({size} bytes) has {} {actual}, expected {}; the partial file was deleted",
                    artifact.url,
                    artifact.hash.algorithm(),
                    artifact.hash
                ),
            ));
        }
        let dir = entry.parent().expect("an entry path has a parent");
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", dir.display()), e))?;
        std::fs::rename(part, entry).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot publish {} as {}", part.display(), entry.display()),
                e,
            )
        })?;
        remove_if_present(&self.sidecar_path(artifact))
    }
}

// Helpers =============================================================================================================

fn unexpected_status(url: &str, status: u16) -> Error {
    Error::new(
        ErrorKind::Download,
        format!("{url} answered HTTP status {status}; any partial file is kept for a retry"),
    )
}

/// The size of the whole file, when the answer says.
fn total_size(response: &Response, from: u64) -> Option<u64> {
    match response.content_range {
        Some(ContentRange::Range { total: Some(total), .. }) => Some(total),
        _ => response.content_length.map(|length| from + length),
    }
}

fn file_len_opt(path: &Path) -> Result<Option<u64>> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some(meta.len())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io(ErrorKind::Io, format!("cannot stat {}", path.display()), e)),
    }
}

fn file_len(path: &Path) -> Result<u64> {
    Ok(file_len_opt(path)?.unwrap_or(0))
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io(ErrorKind::Io, format!("cannot delete {}", path.display()), e)),
    }
}

/// The running hash of a download, in the algorithm its artifact is pinned by.
enum Hasher {
    Sha256(Sha2),
    Sha512(Sha2_512),
}

impl Hasher {
    fn new(pinned: &ContentHash) -> Hasher {
        match pinned {
            ContentHash::Sha256(_) => Hasher::Sha256(Sha2::new()),
            ContentHash::Sha512(_) => Hasher::Sha512(Sha2_512::new()),
        }
    }

    /// A hasher primed with the bytes already in `path`.
    fn of_file(pinned: &ContentHash, path: &Path) -> Result<Hasher> {
        let fail = |e| Error::io(ErrorKind::Io, format!("cannot read {}", path.display()), e);
        let file = File::open(path).map_err(fail)?;
        Ok(match pinned {
            ContentHash::Sha256(_) => Hasher::Sha256(hash_reader(file, path, &mut |_| {})?),
            ContentHash::Sha512(_) => Hasher::Sha512(hash_reader(file, path, &mut |_| {})?),
        })
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Hasher::Sha256(hasher) => hasher.update(bytes),
            Hasher::Sha512(hasher) => hasher.update(bytes),
        }
    }

    fn finish(self) -> ContentHash {
        match self {
            Hasher::Sha256(hasher) => Sha256::from_bytes(hasher.finalize().into()).into(),
            Hasher::Sha512(hasher) => Sha512::from_bytes(hasher.finalize().into()).into(),
        }
    }
}

/// The state of `D` after all the bytes `file` holds from where it is; `path` names it in errors, and `done` is told
/// how many bytes were hashed after each read.
pub(crate) fn hash_reader<D: Digest>(mut file: impl Read, path: &Path, done: &mut dyn FnMut(u64)) -> Result<D> {
    let mut hasher = D::new();
    let fail = |e| Error::io(ErrorKind::Io, format!("cannot read {}", path.display()), e);
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = match file.read(&mut buffer) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            read => read.map_err(fail)?,
        };
        if read == 0 {
            return Ok(hasher);
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
        done(total);
    }
}

/// The SHA-256 of the file at `path`.
pub(crate) fn hash_file(path: &Path) -> Result<[u8; 32]> {
    let fail = |e| Error::io(ErrorKind::Io, format!("cannot read {}", path.display()), e);
    let hasher: Sha2 = hash_reader(File::open(path).map_err(fail)?, path, &mut |_| {})?;
    Ok(hasher.finalize().into())
}
