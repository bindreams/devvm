//! The SFTP subsystem of the test server: a directory, plus what tests declare on top of it (see [`super::TestSshd`]).

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Packet, Status, StatusCode, Version,
};
use russh_sftp::server::{Handler, StatusReply};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use super::State;

const POSIX_RENAME: &str = "posix-rename@openssh.com";

/// The pattern byte at `offset`.
pub fn pattern_byte(offset: u64) -> u8 {
    (offset.wrapping_mul(31) ^ (offset >> 8)) as u8
}

/// The size of the file the big-copy tests move: one byte past 4 GiB, so every 32-bit offset limit is crossed.
pub const BIG_SIZE: u64 = (1 << 32) + 1;

/// The byte ranges whose content is checked in a big file: 64 KiB every 256 MiB, and the last 32 KiB (which straddle
/// 4 GiB). Hashing every byte of 4 GiB would take minutes in a debug build; the length is always exact.
pub fn sample_windows() -> Vec<(u64, u64)> {
    let mut windows: Vec<(u64, u64)> = (0..16u64)
        .map(|k| (k * (256 << 20), k * (256 << 20) + (64 << 10)))
        .collect();
    windows.push((BIG_SIZE - (32 << 10), BIG_SIZE));
    windows
}

/// The hex SHA-256 over the sample windows of a [`BIG_SIZE`] file whose windows hold the pattern.
pub fn expected_big_digest() -> String {
    let mut hasher = Sha256::new();
    for (start, end) in sample_windows() {
        hasher.update((start..end).map(pattern_byte).collect::<Vec<_>>());
    }
    hex::encode(hasher.finalize())
}

/// The hex SHA-256 over the sample windows of the file at `path`.
pub fn big_file_digest(path: &std::path::Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    for (start, end) in sample_windows() {
        file.seek(SeekFrom::Start(start))?;
        let mut window = vec![0u8; (end - start) as usize];
        file.read_exact(&mut window)?;
        hasher.update(&window);
    }
    Ok(hex::encode(hasher.finalize()))
}

enum Open {
    File {
        file: std::fs::File,
        path: String,
    },
    /// Writes are hashed and dropped.
    Sink {
        path: String,
        hasher: Sha256,
        len: u64,
    },
    /// Reads are generated.
    Source {
        size: u64,
    },
    Dir {
        entries: Vec<File>,
        sent: bool,
    },
}

pub(super) struct SftpFs {
    state: Arc<State>,
    drop_now: Arc<Notify>,
    handles: HashMap<String, Open>,
    next_handle: u64,
}

fn failure(message: impl Into<String>) -> StatusReply {
    StatusCode::Failure.with_message(message)
}

fn from_io(e: std::io::Error) -> StatusReply {
    match e.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile.into(),
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied.into(),
        _ => failure(e.to_string()),
    }
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_owned(),
        language_tag: "en-US".to_owned(),
    }
}

/// `path` as an absolute `/`-separated path without `.` and `..`; relative paths are relative to `/`.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

fn parent(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(i) => path[..i].to_owned(),
    }
}

/// The error a data operation on the open file `path` gets, if its directory is declared failing.
fn data_failure(state: &State, path: &str) -> Result<(), StatusReply> {
    if state.failing_data.lock().unwrap().contains(&parent(path)) {
        return Err(failure("No space left on device"));
    }
    Ok(())
}

impl SftpFs {
    pub(super) fn new(state: Arc<State>, drop_now: Arc<Notify>) -> SftpFs {
        SftpFs {
            state,
            drop_now,
            handles: HashMap::new(),
            next_handle: 0,
        }
    }

    fn real(&self, path: &str) -> PathBuf {
        path.split('/')
            .filter(|part| !part.is_empty())
            .fold(self.state.root.clone(), |dir, part| dir.join(part))
    }

    /// Follows virtual symlinks: the last component if `follow`, and every directory component always. A loop is an
    /// error.
    fn resolve(&self, path: &str, follow: bool) -> Result<String, StatusReply> {
        let mut current = normalize(path);
        for _ in 0..40 {
            let parts: Vec<&str> = current.split('/').filter(|part| !part.is_empty()).collect();
            let hit = {
                let links = self.state.links.lock().unwrap();
                (1..=parts.len()).filter(|&i| i < parts.len() || follow).find_map(|i| {
                    let prefix = format!("/{}", parts[..i].join("/"));
                    links.get(&prefix).map(|target| (i, prefix, target.clone()))
                })
            };
            let Some((i, prefix, target)) = hit else {
                return Ok(current);
            };
            let replaced = if target.starts_with('/') {
                normalize(&target)
            } else {
                normalize(&format!("{}/{target}", parent(&prefix)))
            };
            current = normalize(&format!("{replaced}/{}", parts[i..].join("/")));
        }
        Err(failure("too many levels of symbolic links"))
    }

    /// Whether the entries of the directory holding `path` cannot be changed.
    fn is_locked(&self, path: &str) -> bool {
        self.state.read_only_dirs.lock().unwrap().contains(&parent(path))
    }

    fn is_discarding(&self, path: &str) -> bool {
        self.state.discarding.lock().unwrap().contains(&parent(path))
    }

    /// The attributes of the virtual `path`, following a symlink if `follow`.
    fn attributes(&self, path: &str, follow: bool) -> Result<FileAttributes, StatusReply> {
        let path = self.resolve(path, follow)?;
        let mut attrs = FileAttributes::default();
        let mode = self.state.modes.lock().unwrap().get(&path).copied();
        if let Some(target) = self.state.links.lock().unwrap().get(&path) {
            attrs.size = Some(target.len() as u64);
            attrs.permissions = Some(0o120777);
        } else if let Some(size) = self.state.generated.lock().unwrap().get(&path) {
            attrs.size = Some(*size);
            attrs.permissions = Some(0o100000 | mode.unwrap_or(0o644));
        } else if let Some((len, _)) = self.state.sinks.lock().unwrap().get(&path) {
            attrs.size = Some(*len);
            attrs.permissions = Some(0o100000 | mode.unwrap_or(0o644));
        } else {
            let metadata = std::fs::metadata(self.real(&path)).map_err(from_io)?;
            attrs.size = Some(metadata.len());
            attrs.permissions = Some(if metadata.is_dir() {
                0o040000 | mode.unwrap_or(0o755)
            } else {
                0o100000 | mode.unwrap_or(0o644)
            });
        }
        Ok(attrs)
    }

    fn new_handle(&mut self, open: Open) -> String {
        self.next_handle += 1;
        let handle = format!("h{}", self.next_handle);
        self.handles.insert(handle.clone(), open);
        handle
    }

    /// Counts `len` transferred bytes; past the limit the connection is dropped.
    fn transferred(&self, len: usize) -> Result<(), StatusReply> {
        let total = self.state.transferred.fetch_add(len as u64, Ordering::SeqCst) + len as u64;
        if self.state.options.kill_sftp_after.is_some_and(|limit| total >= limit) {
            self.drop_now.notify_one();
            return Err(failure("the test server drops the connection here"));
        }
        Ok(())
    }

    /// Moves what the server knows about `from` to `to`.
    fn move_records(&self, from: &str, to: &str) {
        let mut modes = self.state.modes.lock().unwrap();
        if let Some(mode) = modes.remove(from) {
            modes.insert(to.to_owned(), mode);
        }
        let mut sinks = self.state.sinks.lock().unwrap();
        if let Some(sink) = sinks.remove(from) {
            sinks.insert(to.to_owned(), sink);
        }
    }

    fn rename_path(&self, old: &str, new: &str, replace: bool) -> Result<(), StatusReply> {
        let (old, new) = (normalize(old), normalize(new));
        if self.is_locked(&new) || self.is_locked(&old) {
            return Err(StatusCode::PermissionDenied.into());
        }
        let exists = self.attributes(&new, false).is_ok();
        if exists && !replace {
            return Err(failure("the destination exists"));
        }
        if self.state.sinks.lock().unwrap().contains_key(&old) {
            self.move_records(&old, &new);
            return Ok(());
        }
        std::fs::rename(self.real(&old), self.real(&new)).map_err(from_io)?;
        self.move_records(&old, &new);
        Ok(())
    }
}

/// Two SFTP strings (`u32` length, bytes), as `posix-rename@openssh.com` carries them.
fn two_strings(data: &[u8]) -> Option<(String, String)> {
    let mut rest = data;
    let mut next = || -> Option<String> {
        let (len, tail) = rest.split_first_chunk::<4>()?;
        let len = u32::from_be_bytes(*len) as usize;
        let (text, tail) = (tail.get(..len)?, tail.get(len..)?);
        rest = tail;
        String::from_utf8(text.to_vec()).ok()
    };
    Some((next()?, next()?))
}

impl Handler for SftpFs {
    type Error = StatusReply;

    fn unimplemented(&self) -> StatusReply {
        StatusCode::OpUnsupported.into()
    }

    async fn init(&mut self, _version: u32, _extensions: HashMap<String, String>) -> Result<Version, StatusReply> {
        Ok(Version::new())
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        attrs: FileAttributes,
    ) -> Result<Handle, StatusReply> {
        let path = self.resolve(&filename, true)?;
        let writing = pflags.contains(OpenFlags::WRITE);
        if pflags.contains(OpenFlags::CREATE) && self.is_locked(&path) && !self.real(&path).exists() {
            return Err(StatusCode::PermissionDenied.into());
        }
        let generated = self.state.generated.lock().unwrap().get(&path).copied();
        let open = if let (Some(size), false) = (generated, writing) {
            Open::Source { size }
        } else if writing && self.is_discarding(&path) {
            Open::Sink {
                path: path.clone(),
                hasher: Sha256::new(),
                len: 0,
            }
        } else {
            let file = std::fs::OpenOptions::from(pflags)
                .open(self.real(&path))
                .map_err(from_io)?;
            Open::File {
                file,
                path: path.clone(),
            }
        };
        if writing && let Some(permissions) = attrs.permissions {
            self.state.modes.lock().unwrap().insert(path, permissions & 0o7777);
        }
        Ok(Handle {
            id,
            handle: self.new_handle(open),
        })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, StatusReply> {
        if let Some(Open::Sink { path, hasher, len }) = self.handles.remove(&handle) {
            self.state
                .sinks
                .lock()
                .unwrap()
                .insert(path, (len, hex::encode(hasher.finalize())));
        }
        Ok(ok(id))
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, StatusReply> {
        let data = match self.handles.get_mut(&handle) {
            Some(Open::Source { size }) => {
                if offset >= *size {
                    return Err(StatusCode::Eof.into());
                }
                let count = (*size - offset).min(u64::from(len)) as usize;
                // Zero, except the pattern inside the sample windows.
                let mut data = vec![0u8; count];
                for (start, end) in sample_windows() {
                    for at in start.max(offset)..end.min(offset + count as u64) {
                        data[(at - offset) as usize] = pattern_byte(at);
                    }
                }
                data
            }
            Some(Open::File { file, path }) => {
                data_failure(&self.state, path)?;
                file.seek(SeekFrom::Start(offset)).map_err(from_io)?;
                let mut buffer = vec![0u8; len as usize];
                let read = file.read(&mut buffer).map_err(from_io)?;
                if read == 0 {
                    return Err(StatusCode::Eof.into());
                }
                buffer.truncate(read);
                buffer
            }
            _ => return Err(failure("not a readable handle")),
        };
        self.transferred(data.len())?;
        Ok(Data { id, data })
    }

    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, StatusReply> {
        self.transferred(data.len())?;
        match self.handles.get_mut(&handle) {
            Some(Open::Sink { hasher, len, .. }) => {
                if offset != *len {
                    return Err(failure(format!(
                        "the test sink wants sequential writes, got offset {offset} at {len}"
                    )));
                }
                // Only the sample windows are hashed; the length is counted exactly.
                for (start, end) in sample_windows() {
                    let (from, to) = (start.max(offset), end.min(offset + data.len() as u64));
                    if from < to {
                        hasher.update(&data[(from - offset) as usize..(to - offset) as usize]);
                    }
                }
                *len += data.len() as u64;
            }
            Some(Open::File { file, path }) => {
                data_failure(&self.state, path)?;
                file.seek(SeekFrom::Start(offset)).map_err(from_io)?;
                file.write_all(&data).map_err(from_io)?;
            }
            _ => return Err(failure("not a writable handle")),
        }
        Ok(ok(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusReply> {
        Ok(Attrs {
            id,
            attrs: self.attributes(&path, false)?,
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, StatusReply> {
        Ok(Attrs {
            id,
            attrs: self.attributes(&path, true)?,
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, StatusReply> {
        let attrs = match self.handles.get(&handle) {
            Some(Open::File { path, .. }) => self.attributes(path, true)?,
            Some(Open::Sink { len, .. }) => FileAttributes {
                size: Some(*len),
                permissions: Some(0o100644),
                ..FileAttributes::default()
            },
            Some(Open::Source { size }) => FileAttributes {
                size: Some(*size),
                permissions: Some(0o100644),
                ..FileAttributes::default()
            },
            _ => return Err(failure("not a file handle")),
        };
        Ok(Attrs { id, attrs })
    }

    async fn setstat(&mut self, id: u32, path: String, attrs: FileAttributes) -> Result<Status, StatusReply> {
        let path = self.resolve(&path, true)?;
        if let Some(permissions) = attrs.permissions {
            self.state.modes.lock().unwrap().insert(path, permissions & 0o7777);
        }
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, StatusReply> {
        let dir = self.resolve(&path, true)?;
        let mut names = BTreeSet::new();
        for entry in std::fs::read_dir(self.real(&dir)).map_err(from_io)? {
            names.insert(entry.map_err(from_io)?.file_name().to_string_lossy().into_owned());
        }
        let child = |full: &String| {
            (parent(full) == dir && full != "/").then(|| full[full.rfind('/').unwrap() + 1..].to_owned())
        };
        names.extend(self.state.links.lock().unwrap().keys().filter_map(child));
        names.extend(self.state.generated.lock().unwrap().keys().filter_map(child));
        names.extend(self.state.sinks.lock().unwrap().keys().filter_map(child));
        let phantoms: Vec<String> = self.state.phantoms.lock().unwrap().iter().filter_map(child).collect();
        names.extend(phantoms.iter().cloned());
        let mut entries = Vec::new();
        for name in names {
            let full = if dir == "/" {
                format!("/{name}")
            } else {
                format!("{dir}/{name}")
            };
            let attrs = if phantoms.contains(&name) {
                FileAttributes::default()
            } else {
                self.attributes(&full, false)?
            };
            entries.push(File::new(name, attrs));
        }
        Ok(Handle {
            id,
            handle: self.new_handle(Open::Dir { entries, sent: false }),
        })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, StatusReply> {
        match self.handles.get_mut(&handle) {
            Some(Open::Dir { entries, sent }) if !*sent => {
                *sent = true;
                Ok(Name {
                    id,
                    files: std::mem::take(entries),
                })
            }
            Some(Open::Dir { .. }) => Err(StatusCode::Eof.into()),
            _ => Err(failure("not a directory handle")),
        }
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, StatusReply> {
        let path = self.resolve(&filename, false)?;
        if self.is_locked(&path) {
            return Err(StatusCode::PermissionDenied.into());
        }
        if self.state.links.lock().unwrap().remove(&path).is_some()
            || self.state.sinks.lock().unwrap().remove(&path).is_some()
            || self.state.generated.lock().unwrap().remove(&path).is_some()
        {
            return Ok(ok(id));
        }
        std::fs::remove_file(self.real(&path)).map_err(from_io)?;
        Ok(ok(id))
    }

    async fn mkdir(&mut self, id: u32, path: String, _attrs: FileAttributes) -> Result<Status, StatusReply> {
        if self.is_locked(&normalize(&path)) {
            return Err(StatusCode::PermissionDenied.into());
        }
        std::fs::create_dir(self.real(&normalize(&path))).map_err(from_io)?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, StatusReply> {
        std::fs::remove_dir(self.real(&normalize(&path))).map_err(from_io)?;
        Ok(ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, StatusReply> {
        Ok(Name {
            id,
            files: vec![File::dummy(self.resolve(&path, true)?)],
        })
    }

    async fn rename(&mut self, id: u32, oldpath: String, newpath: String) -> Result<Status, StatusReply> {
        self.rename_path(&oldpath, &newpath, false)?;
        Ok(ok(id))
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, StatusReply> {
        let path = self.resolve(&path, false)?;
        match self.state.links.lock().unwrap().get(&path) {
            Some(target) => Ok(Name {
                id,
                files: vec![File::dummy(target.clone())],
            }),
            None => Err(failure("not a symlink")),
        }
    }

    async fn symlink(&mut self, id: u32, linkpath: String, targetpath: String) -> Result<Status, StatusReply> {
        self.state
            .links
            .lock()
            .unwrap()
            .insert(normalize(&linkpath), targetpath);
        Ok(ok(id))
    }

    async fn extended(&mut self, id: u32, request: String, data: Vec<u8>) -> Result<Packet, StatusReply> {
        if request != POSIX_RENAME {
            return Err(StatusCode::OpUnsupported.into());
        }
        let (old, new) = two_strings(&data).ok_or_else(|| failure("malformed posix-rename request"))?;
        self.rename_path(&old, &new, true)?;
        Ok(Packet::Status(ok(id)))
    }
}
