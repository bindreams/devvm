//! The `ssh_config` and `known_hosts` files that let a plain `ssh` (and VS Code Remote-SSH) reach an instance without a
//! trust prompt.
//!
//! `<dir>/<name>.conf` is one instance's entry; `<dir>/config` is static and includes them all. Every change happens
//! under the OS lock `<dir>/.lock`, so a sweep that found no instance for a name cannot delete the conf of an instance
//! of that name placed a moment later. Order in `up`: the instance is placed, then its conf is written.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::exit::{NullProgress, Progress};
use crate::instance::name::InstanceName;
use crate::instance::record::{InstanceId, write_file};
use crate::store::lock::{FileLock, Mode};
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "ssh_config_tests.rs"]
mod ssh_config_tests;

const CONFIG: &str = "config";
const CONF_SUFFIX: &str = ".conf";
/// What `write_file` appends to a name for the file it writes first.
const TEMP_MARK: &str = ".tmp-";

/// What `<name>.conf` says about one instance.
pub(crate) struct SshConfigEntry {
    pub name: InstanceName,
    pub id: InstanceId,
    pub port: u16,
    pub identity: PathBuf,
    /// Where this instance's `known_hosts` is written. The caller owns the file's lifetime; the entry only points to it.
    pub known_hosts: PathBuf,
}

/// `<state>/ssh`.
pub(crate) struct SshConfigDir {
    dir: PathBuf,
    /// Told "waiting for the ssh configuration" when another process holds the lock.
    progress: Arc<dyn Progress>,
}

fn io(what: &str, path: &Path, source: std::io::Error) -> Error {
    Error::io(ErrorKind::Io, format!("cannot {what} {}", path.display()), source)
}

/// `path` as ssh_config spells it: separators are `/` (OpenSSH for Windows reads both, and a backslash would need
/// escaping), and a non-Unicode path is refused.
fn ssh_path(path: &Path) -> Result<String> {
    let unicode = |s: &std::ffi::OsStr| {
        s.to_str().map(str::to_owned).ok_or_else(|| {
            Error::new(
                ErrorKind::Io,
                format!("{} is not valid Unicode, so ssh_config cannot name it", path.display()),
            )
        })
    };
    let mut out = String::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push_str(&unicode(prefix.as_os_str())?),
            Component::RootDir => out.push('/'),
            other => {
                if !out.is_empty() && !out.ends_with('/') {
                    out.push('/');
                }
                out.push_str(&unicode(other.as_os_str())?);
            }
        }
    }
    Ok(out)
}

/// `path` as ssh reads it back unchanged from a value it expands: `IdentityFile`, `UserKnownHostsFile` and `Include`
/// treat `%x` as a token (`%%` is a literal `%`) and `${NAME}` as an environment variable, which has no escape.
fn expansion_literal(path: &str) -> Result<String> {
    if path.contains("${") {
        return Err(Error::new(
            ErrorKind::Io,
            format!("{path:?} contains `${{`, which ssh_config reads as an environment variable and cannot escape"),
        ));
    }
    Ok(path.replace('%', "%%"))
}

/// `path` as a glob that matches only that path: each of `[`, `]`, `*` and `?` in a one-character class.
fn glob_literal(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '[' | ']' | '*' | '?' => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            other => out.push(other),
        }
    }
    out
}

/// `text` in double quotes, with `\` and `"` escaped, so a space or any other character is part of the value.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn conf_text(e: &SshConfigEntry) -> Result<String> {
    Ok(format!(
        "# Written by devvm; rewritten whenever the instance starts.\n\
         Host {name}\n\
         \x20   HostName 127.0.0.1\n\
         \x20   Port {port}\n\
         \x20   User user\n\
         \x20   IdentityFile {identity}\n\
         \x20   IdentitiesOnly yes\n\
         \x20   UserKnownHostsFile {known_hosts}\n\
         \x20   HostKeyAlias devvm-{id}\n\
         \x20   StrictHostKeyChecking yes\n",
        name = e.name,
        port = e.port,
        identity = quoted(&expansion_literal(&ssh_path(&e.identity)?)?),
        known_hosts = quoted(&expansion_literal(&ssh_path(&e.known_hosts)?)?),
        id = e.id,
    ))
}

/// The one `known_hosts` line: the alias, the key type and the key, without the key's comment.
fn known_hosts_text(id: &InstanceId, host_public: &str) -> Result<String> {
    let mut fields = host_public.split_whitespace();
    match (fields.next(), fields.next()) {
        (Some(kind), Some(key)) => Ok(format!("devvm-{id} {kind} {key}\n")),
        _ => Err(Error::new(
            ErrorKind::Internal,
            format!("{host_public:?} is not an OpenSSH public key"),
        )),
    }
}

/// Writes `text` to `path` through a temporary file and a rename, unless the file already holds exactly `text`.
fn write_if_different(path: &Path, text: &str) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == text) {
        return Ok(());
    }
    let (Some(dir), Some(file)) = (path.parent(), path.file_name().and_then(|f| f.to_str())) else {
        return Err(Error::new(
            ErrorKind::Internal,
            format!("{} is not a file path", path.display()),
        ));
    };
    std::fs::create_dir_all(dir).map_err(|e| io("create", dir, e))?;
    write_file(dir, file, text)
}

impl SshConfigDir {
    pub(crate) fn new(dir: PathBuf) -> Self {
        SshConfigDir {
            dir,
            progress: Arc::new(NullProgress),
        }
    }

    pub(crate) fn with_progress(mut self, progress: Arc<dyn Progress>) -> Self {
        self.progress = progress;
        self
    }

    pub(crate) fn path(&self) -> &Path {
        &self.dir
    }

    fn lock(&self) -> Result<FileLock> {
        let mut lock = FileLock::open(&self.dir.join(".lock"))?;
        lock.lock(Mode::Exclusive, "the ssh configuration", &*self.progress)?;
        Ok(lock)
    }

    fn conf_path(&self, name: &InstanceName) -> PathBuf {
        self.dir.join(format!("{name}{CONF_SUFFIX}"))
    }

    /// Writes `<name>.conf`, and the entry's `known_hosts` when its content differs (it holds the key under an alias
    /// without the port, so a new port rewrites only the conf). The `known_hosts` file comes first: a conf never
    /// points to a file that is not there.
    pub(crate) fn write_entry(&self, e: &SshConfigEntry, host_public: &str) -> Result<()> {
        let conf = conf_text(e)?;
        let known_hosts = known_hosts_text(&e.id, host_public)?;
        let _lock = self.lock()?;
        write_if_different(&e.known_hosts, &known_hosts)?;
        write_if_different(&self.conf_path(&e.name), &conf)
    }

    /// Removes `<name>.conf`; an entry that is not there is fine.
    pub(crate) fn remove_entry(&self, name: &InstanceName) -> Result<()> {
        let _lock = self.lock()?;
        let path = self.conf_path(name);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io("remove", &path, e)),
        }
    }

    /// Writes the static `config` that includes every `<name>.conf`, unless it is already exactly that.
    pub(crate) fn ensure_top_level(&self) -> Result<()> {
        let dir = glob_literal(&expansion_literal(&ssh_path(&self.dir)?)?);
        let glob = format!("{dir}/*{CONF_SUFFIX}");
        let text = format!("Include {}\n", quoted(&glob));
        let _lock = self.lock()?;
        write_if_different(&self.dir.join(CONFIG), &text)
    }

    /// Removes the conf of every instance `live` denies, and the temporary files a killed writer left. Returns what it
    /// could not do, as warnings. Files that are not devvm's (a name that is not an instance name) are left alone.
    ///
    /// `live` runs under this directory's lock. If it takes another lock (the instance registry's), every caller of
    /// this type's methods must take that lock first, or a writer holding it and waiting for this directory deadlocks
    /// with the sweep.
    pub(crate) fn sweep(&self, live: &dyn Fn(&InstanceName) -> bool) -> Vec<String> {
        let mut warnings = Vec::new();
        let _lock = match self.lock() {
            Ok(lock) => lock,
            Err(e) => return vec![format!("cannot sweep the ssh configuration: {}", e.message())],
        };
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return warnings,
            Err(e) => return vec![io("list", &self.dir, e).message().to_owned()],
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    warnings.push(io("list", &self.dir, e).message().to_owned());
                    continue;
                }
            };
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else { continue };
            let remove = if let Some(stem) = file_name.strip_suffix(CONF_SUFFIX) {
                match InstanceName::parse(stem) {
                    Ok(name) if name.as_str() == stem => !live(&name),
                    _ => false,
                }
            } else {
                file_name.contains(&format!("{CONF_SUFFIX}{TEMP_MARK}"))
            };
            if remove && let Err(e) = std::fs::remove_file(entry.path()) {
                warnings.push(io("remove", &entry.path(), e).message().to_owned());
            }
        }
        warnings
    }
}
