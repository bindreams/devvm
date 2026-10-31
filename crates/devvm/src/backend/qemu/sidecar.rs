//! devvm's own QEMU build: found in the cache, or downloaded, extracted and published once.
//!
//! On disk, under `Dirs::qemu()`:
//!
//! - `<version>-<12 hex of the zip sha256>/`: a published install (`bin/`, `share/`). It exists only once complete,
//!   because it is published by one rename of a finished extraction directory;
//! - `.extract-<random>/`: an extraction in progress; only a killed extraction leaves one behind;
//! - `.extract.lock`: one OS lock around extraction and publication.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Deserialize;

use crate::config::Config;
use crate::exit::Progress;
use crate::host::{Host, SystemDirs};
use crate::store::fetch::{Artifact, Fetcher};
use crate::store::lock::{FileLock, Mode};
use crate::{Dirs, Error, ErrorKind, Result, Sha256};

#[cfg(test)]
#[path = "sidecar_tests.rs"]
pub(crate) mod sidecar_tests;

const MANIFEST: &str = include_str!("../../../qemu-manifest.toml");

/// Tools and firmware a downloaded build must hold. A configured QEMU needs only the amd64 ones.
const ARCHIVE_TOOLS: &[&str] = &["qemu-img", "qemu-system-x86_64", "qemu-system-aarch64"];
const ARCHIVE_FIRMWARE: &[&str] = &[
    "edk2-x86_64-code.fd",
    "edk2-aarch64-code.fd",
    "edk2-i386-vars.fd",
    "edk2-arm-vars.fd",
];
const CONFIGURED_TOOLS: &[&str] = &["qemu-img", "qemu-system-x86_64"];
const CONFIGURED_FIRMWARE: &[&str] = &["edk2-x86_64-code.fd", "edk2-i386-vars.fd"];
/// The tools run with `--version` before an extraction is published.
const VERSIONED_TOOLS: &[&str] = &["qemu-img", "qemu-system-x86_64"];

// Manifest ============================================================================================================

/// One target's pinned build, as `qemu-manifest.toml` states it.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ManifestEntry {
    pub qemu_version: String,
    pub url: String,
    pub sha256: Sha256,
}

impl ManifestEntry {
    pub(crate) fn artifact(&self) -> Artifact {
        Artifact {
            url: self.url.clone(),
            sha256: self.sha256,
        }
    }
}

#[derive(Deserialize)]
struct Manifest {
    targets: BTreeMap<String, ManifestEntry>,
}

/// The pinned build for `triple`; `Unavailable` naming the triple when devvm publishes none.
pub(crate) fn manifest_entry(triple: &str) -> Result<ManifestEntry> {
    let mut manifest: Manifest = toml::from_str(MANIFEST).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            format!("the embedded qemu-manifest.toml is invalid: {e}"),
        )
    })?;
    manifest.targets.remove(triple).ok_or_else(|| {
        Error::new(
            ErrorKind::Unavailable,
            format!("devvm publishes no QEMU build for {triple}; set qemu.path in the configuration to use your own"),
        )
    })
}

// Install =============================================================================================================

/// A QEMU installation: where its tools and firmware are.
#[derive(Clone, Debug)]
pub(crate) struct QemuInstall {
    pub bin: PathBuf,
    pub share: PathBuf,
    pub version: String,
    /// The sha256 of the downloaded archive; `None` for a configured QEMU.
    pub zip_sha256: Option<Sha256>,
}

impl QemuInstall {
    /// The executable `name` (`qemu-img`, `qemu-system-<arch>`).
    pub(crate) fn tool(&self, name: &str) -> PathBuf {
        self.bin.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
    }

    /// A command for the tool `name` that runs it as QEMU itself is run: nothing in its environment but
    /// [`QemuInstall::environment`], no input, and no console window.
    pub(crate) fn command(&self, name: &str, host: &dyn Host) -> Result<std::process::Command> {
        let mut command = std::process::Command::new(self.tool(name));
        command
            .env_clear()
            .envs(self.environment(host.system_dirs().as_ref())?)
            .stdin(std::process::Stdio::null());
        host.quiet(&mut command);
        Ok(command)
    }

    /// The whole environment QEMU is started with: `SystemRoot`, and `PATH` of System32 then `bin`. Pure: the
    /// Windows directories come from the host. With `None` (no Windows directories here) `PATH` is `bin` alone.
    pub(crate) fn environment(&self, system: Option<&SystemDirs>) -> Result<Vec<(OsString, OsString)>> {
        let join = |dirs: Vec<&Path>| {
            std::env::join_paths(dirs).map_err(|e| {
                Error::new(
                    ErrorKind::Io,
                    format!("cannot build PATH for QEMU from {}: {e}", self.bin.display()),
                )
            })
        };
        Ok(match system {
            Some(system) => vec![
                ("SystemRoot".into(), system.root.clone().into_os_string()),
                ("PATH".into(), join(vec![&system.system32, &self.bin])?),
            ],
            None => vec![("PATH".into(), join(vec![&self.bin])?)],
        })
    }
}

/// Runs the tool `name` of an install with `--version` and returns what it printed.
pub(crate) type Verify = Arc<dyn Fn(&QemuInstall, &str) -> Result<String> + Send + Sync>;

/// Runs the tool as QEMU runs it: its environment, and an empty working directory that nothing else can add a library to.
fn run_version(host: &dyn Host, install: &QemuInstall, name: &str) -> Result<String> {
    let tool = install.tool(name);
    let cwd = tempfile::tempdir().map_err(|e| {
        Error::io(
            ErrorKind::Io,
            format!("cannot create a working directory for {} --version", tool.display()),
            e,
        )
    })?;
    let output = install
        .command(name, host)?
        .arg("--version")
        .current_dir(cwd.path())
        .output()
        .map_err(|e| Error::io(ErrorKind::Store, format!("cannot run {} --version", tool.display()), e))?;
    if !output.status.success() {
        return Err(Error::new(
            ErrorKind::Store,
            format!(
                "{} --version failed ({}): {}",
                tool.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Where an install comes from: the configuration, or the manifest's pinned build.
pub(crate) struct Source {
    dirs: Dirs,
    config: Config,
    fetcher: Arc<Fetcher>,
    progress: Arc<dyn Progress>,
    entry: Result<ManifestEntry>,
    verify: Verify,
}

impl Source {
    pub(crate) fn new(
        dirs: Dirs,
        config: Config,
        fetcher: Arc<Fetcher>,
        progress: Arc<dyn Progress>,
        entry: Result<ManifestEntry>,
        verify: Verify,
    ) -> Source {
        Source {
            dirs,
            config,
            fetcher,
            progress,
            entry,
            verify,
        }
    }

    fn entry(&self) -> Result<&ManifestEntry> {
        self.entry
            .as_ref()
            .map_err(|e| Error::new(e.kind(), e.message().to_owned()))
    }

    /// `<qemu>/<version>-<12 hex of the zip sha256>`.
    fn published(&self, entry: &ManifestEntry) -> PathBuf {
        self.dirs
            .qemu()
            .join(format!("{}-{}", entry.qemu_version, &entry.sha256.to_string()[..12]))
    }

    fn install_at(&self, root: &Path, entry: &ManifestEntry) -> QemuInstall {
        QemuInstall {
            bin: root.join("bin"),
            share: root.join("share"),
            version: entry.qemu_version.clone(),
            zip_sha256: Some(entry.sha256),
        }
    }

    /// The install, downloading and extracting it first if it is not in the cache.
    pub(crate) fn resolve(&self) -> Result<QemuInstall> {
        match &self.config.qemu_path {
            Some(dir) => configured(dir),
            None => self.download(),
        }
    }

    /// The install if it is already there; never downloads.
    pub(crate) fn existing(&self) -> Result<Option<QemuInstall>> {
        if let Some(dir) = &self.config.qemu_path {
            return configured(dir).map(Some);
        }
        let entry = self.entry()?;
        let published = self.published(entry);
        Ok(published.is_dir().then(|| self.install_at(&published, entry)))
    }

    fn download(&self) -> Result<QemuInstall> {
        let entry = self.entry()?;
        let published = self.published(entry);
        if published.is_dir() {
            return Ok(self.install_at(&published, entry));
        }
        let zip = self.fetcher.fetch(&entry.artifact())?;

        let qemu = self.dirs.qemu();
        std::fs::create_dir_all(&qemu)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", qemu.display()), e))?;
        let mut lock = FileLock::open(&qemu.join(".extract.lock"))?;
        lock.lock(
            Mode::Exclusive,
            "another devvm process to extract QEMU",
            &*self.progress,
        )?;
        // No extraction runs without the lock, so every `.extract-*` directory is a killed extraction's leftover.
        remove_stale_extractions(&qemu)?;
        if published.is_dir() {
            return Ok(self.install_at(&published, entry));
        }

        self.progress
            .message(&format!("extracting QEMU {}", entry.qemu_version));
        let staging = tempfile::Builder::new()
            .prefix(".extract-")
            .tempdir_in(&qemu)
            .map_err(|e| {
                Error::io(
                    ErrorKind::Io,
                    format!("cannot create a directory in {}", qemu.display()),
                    e,
                )
            })?;
        extract(&zip, staging.path())?;
        let install = self.install_at(staging.path(), entry);
        require_tools(&install.bin, ARCHIVE_TOOLS, ErrorKind::Store)?;
        require_firmware(&install.share, ARCHIVE_FIRMWARE, ErrorKind::Store)?;
        for tool in VERSIONED_TOOLS {
            let printed = (self.verify)(&install, tool)?;
            if !printed.contains(&entry.qemu_version) {
                return Err(Error::new(
                    ErrorKind::Store,
                    format!(
                        "{tool} in {} should be QEMU {} but printed {:?}",
                        entry.url,
                        entry.qemu_version,
                        printed.trim()
                    ),
                ));
            }
        }
        // Publishing happens only under the extraction lock, after the re-check above: nobody else has published it.
        debug_assert!(
            !published.is_dir(),
            "{} appeared under the extraction lock",
            published.display()
        );
        std::fs::rename(staging.path(), &published).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot publish QEMU as {}", published.display()),
                e,
            )
        })?;
        Ok(self.install_at(&published, entry))
    }
}

/// A QEMU the user installed: `dir` holds the executables, the firmware is in `<dir>/share` or `<dir>/../share`.
fn configured(dir: &Path) -> Result<QemuInstall> {
    if !dir.is_absolute() {
        return Err(Error::new(
            ErrorKind::Unavailable,
            format!(
                "the setting qemu.path is {}, which is not an absolute path: QEMU runs in an empty working directory, \
                 so a relative path would not find its firmware",
                dir.display()
            ),
        ));
    }
    let install = |share: PathBuf| QemuInstall {
        bin: dir.to_path_buf(),
        share,
        version: "configured".to_owned(),
        zip_sha256: None,
    };
    let beside = install(dir.join("share"));
    require_tools(dir, CONFIGURED_TOOLS, ErrorKind::Unavailable)?;
    let above = dir.parent().map(|parent| parent.join("share"));
    for share in std::iter::once(beside.share.clone()).chain(above.clone()) {
        if firmware_present(&share) {
            return Ok(install(share));
        }
    }
    Err(Error::new(
        ErrorKind::Unavailable,
        format!(
            "the configured QEMU in {} has no firmware ({}) in {} or {}",
            dir.display(),
            CONFIGURED_FIRMWARE.join(", "),
            beside.share.display(),
            above.map_or_else(
                || "its parent directory".to_owned(),
                |share| share.display().to_string()
            )
        ),
    ))
}

fn firmware_present(share: &Path) -> bool {
    CONFIGURED_FIRMWARE.iter().all(|name| share.join(name).is_file())
}

/// Every tool in `tools` is a file in `bin`, else `kind` naming the first missing one.
fn require_tools(bin: &Path, tools: &[&str], kind: ErrorKind) -> Result<()> {
    for tool in tools {
        let path = bin.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX));
        if !path.is_file() {
            return Err(Error::new(
                kind,
                format!("QEMU is incomplete: {} is missing", path.display()),
            ));
        }
    }
    Ok(())
}

/// Every name in `firmware` is a file in `share`, else `kind` naming the first missing one.
fn require_firmware(share: &Path, firmware: &[&str], kind: ErrorKind) -> Result<()> {
    for name in firmware {
        let path = share.join(name);
        if !path.is_file() {
            return Err(Error::new(
                kind,
                format!("QEMU is incomplete: {} is missing", path.display()),
            ));
        }
    }
    Ok(())
}

fn remove_stale_extractions(qemu: &Path) -> Result<()> {
    let read =
        std::fs::read_dir(qemu).map_err(|e| Error::io(ErrorKind::Io, format!("cannot list {}", qemu.display()), e))?;
    for entry in read {
        let entry = entry.map_err(|e| Error::io(ErrorKind::Io, format!("cannot list {}", qemu.display()), e))?;
        if entry.file_name().to_string_lossy().starts_with(".extract-") {
            std::fs::remove_dir_all(entry.path()).map_err(|e| {
                Error::io(
                    ErrorKind::Io,
                    format!("cannot remove the abandoned extraction {}", entry.path().display()),
                    e,
                )
            })?;
        }
    }
    Ok(())
}

/// Unpacks `zip` into `dest`. An entry that would land outside `dest`, and a symbolic link, are refused.
fn extract(zip: &Path, dest: &Path) -> Result<()> {
    let store = |what: String, source: &dyn std::fmt::Display| {
        Error::new(
            ErrorKind::Store,
            format!("cannot extract {}: {what}: {source}", zip.display()),
        )
    };
    let file =
        std::fs::File::open(zip).map_err(|e| Error::io(ErrorKind::Io, format!("cannot open {}", zip.display()), e))?;
    let mut archive =
        zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| store("not a zip archive".to_owned(), &e))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| store(format!("entry {index}"), &e))?;
        let name = String::from_utf8_lossy(entry.name_raw()).into_owned();
        let Some(relative) = entry.enclosed_name() else {
            return Err(Error::new(
                ErrorKind::Store,
                format!(
                    "{} holds the entry {name:?}, which is outside the extraction directory",
                    zip.display()
                ),
            ));
        };
        if entry.is_symlink() {
            return Err(Error::new(
                ErrorKind::Store,
                format!("{} holds the symbolic link {name:?}", zip.display()),
            ));
        }
        let target = dest.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", target.display()), e))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", parent.display()), e))?;
        }
        let mut out = std::fs::File::create(&target)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", target.display()), e))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot extract {name:?} to {}", target.display()),
                e,
            )
        })?;
    }
    Ok(())
}

// Lazy install ========================================================================================================

/// Resolves the install on first use, so nothing downloads until a command needs a QEMU tool or the firmware.
pub(crate) struct LazyInstall {
    source: Source,
    resolved: Mutex<Option<Arc<QemuInstall>>>,
}

impl LazyInstall {
    pub(crate) fn new(
        dirs: Dirs,
        config: Config,
        fetcher: Arc<Fetcher>,
        progress: Arc<dyn Progress>,
        host: &'static dyn Host,
    ) -> LazyInstall {
        LazyInstall::from_source(Source::new(
            dirs,
            config,
            fetcher,
            progress,
            manifest_entry(crate::TARGET),
            Arc::new(move |install, name| run_version(host, install, name)),
        ))
    }

    pub(crate) fn from_source(source: Source) -> LazyInstall {
        LazyInstall {
            source,
            resolved: Mutex::new(None),
        }
    }

    /// The install, resolved once: threads calling at the same time wait for the first. A failure is not cached.
    pub(crate) fn get(&self) -> Result<Arc<QemuInstall>> {
        let mut resolved = self.resolved.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(install) = &*resolved {
            return Ok(install.clone());
        }
        let install = Arc::new(self.source.resolve()?);
        *resolved = Some(install.clone());
        Ok(install)
    }

    /// The install if it is already there; never downloads.
    pub(crate) fn existing(&self) -> Result<Option<Arc<QemuInstall>>> {
        if let Some(install) = &*self.resolved.lock().unwrap_or_else(PoisonError::into_inner) {
            return Ok(Some(install.clone()));
        }
        Ok(self.source.existing()?.map(Arc::new))
    }
}
