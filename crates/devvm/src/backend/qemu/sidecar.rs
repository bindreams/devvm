//! devvm's own QEMU build: found in the cache, or downloaded, extracted and published once.
//!
//! On disk, under `Dirs::qemu()`:
//!
//! - `<version>-<12 hex of the zip sha256>/`: a published install (`bin/`, `share/`). It exists only once complete,
//!   because it is published by one rename of a finished extraction directory. The install the manifest names is the
//!   only one kept: the others are removed under the extraction lock, each by one rename to an `.extract-` name and then
//!   a delete, so an install is removed whole or not at all;
//! - `.extract-<random>/`: an extraction in progress, or an install being removed; a killed devvm leaves one behind;
//! - `.extract.lock`: one OS lock around extraction, publication and removal.
//!
//! An install is in use while a process holds a file inside it open: the [`QemuInstall`] a resolve returns keeps one
//! open ([`HELD_TOOL`], read-only, every sharing allowed) for as long as the value lives, and Windows refuses to rename a
//! directory with a file open in it, whatever the file shares. A launch gives QEMU a duplicate of that handle
//! ([`Host::share_open_file_with`]), which QEMU keeps until it exits, so the install stays held for as long as any
//! process that resolved it lives and any QEMU started from it runs, whether or not the devvm that started it has
//! exited. Removal renames, so an install in use is never touched. No record of which install a machine runs is kept,
//! and none is needed. A configured `qemu.path` is the user's own: nothing is held or given for it. A host where an
//! open file does not stop the rename ([`Host::rename_refused_by_open_file`]) has no such test, and removes nothing
//! there: a manifest entry for another OS needs its own in-use test first.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use super::args::ArchFiles;
use crate::config::Config;
use crate::host::{Host, Sharing, SystemDirs};
use crate::lazy::Lazy;
use crate::progress::{Progress, Total};
use crate::store::fetch::{Artifact, Fetcher};
use crate::store::lock::{FileLock, Mode};
use crate::store::quarantine::quarantine_path;
use crate::{Arch, Dirs, Error, ErrorKind, Result, Sha256};

#[cfg(test)]
#[path = "sidecar_tests.rs"]
pub(crate) mod sidecar_tests;

#[cfg(all(test, windows))]
#[path = "sidecar_windows_tests.rs"]
mod sidecar_windows_tests;

const MANIFEST: &str = include_str!("../../../qemu-manifest.toml");

/// Tools and firmware a downloaded build must hold, for every architecture. A configured QEMU needs the host
/// architecture's ([`ArchFiles`]): the one rule for what a configured `qemu.path` is accepted for, by a launch and by
/// `devvm doctor` alike.
const ARCHIVE_TOOLS: &[&str] = &["qemu-img", "qemu-system-x86_64", "qemu-system-aarch64"];
const ARCHIVE_FIRMWARE: &[&str] = &[
    "edk2-x86_64-code.fd",
    "edk2-aarch64-code.fd",
    "edk2-i386-vars.fd",
    "edk2-arm-vars.fd",
];
/// The tools run with `--version` before an extraction is published.
const VERSIONED_TOOLS: &[&str] = &["qemu-img", "qemu-system-x86_64"];
/// The file of an install that a [`QemuInstall`] keeps open. Every install has it, and it is held with every sharing
/// allowed ([`Sharing::Everyone`]): that it is open is all that protects the install, and nothing that has it open can
/// make the launch fail or stop QEMU and `qemu-img` from starting from the install.
const HELD_TOOL: &str = "qemu-img";
/// The characters of the zip's sha256 an install's directory name carries.
const INSTALL_HASH_LEN: usize = 12;

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
    /// [`HELD_TOOL`] of a downloaded install, open for as long as any copy of this value lives: the proof that the
    /// install was there when the value was made, and what keeps it from being removed. `None` for a configured QEMU.
    pub held: Option<Arc<File>>,
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

/// Where an install comes from: the configuration, or the manifest's pinned build. The configuration is read by the
/// first use that needs it.
pub(crate) struct Source {
    dirs: Dirs,
    config: Lazy<Config>,
    fetcher: Arc<Fetcher>,
    progress: Arc<dyn Progress>,
    entry: Result<ManifestEntry>,
    verify: Verify,
    host: &'static dyn Host,
}

impl Source {
    pub(crate) fn new(
        dirs: Dirs,
        fetcher: Arc<Fetcher>,
        progress: Arc<dyn Progress>,
        entry: Result<ManifestEntry>,
        verify: Verify,
        host: &'static dyn Host,
    ) -> Source {
        Source {
            dirs,
            config: Lazy::new(),
            fetcher,
            progress,
            entry,
            verify,
            host,
        }
    }

    /// The configuration, read from `config.yaml` under the source's `Dirs` by the first call that succeeds.
    pub(crate) fn config(&self) -> Result<&Config> {
        self.config.get(|| Config::load(&self.dirs))
    }

    fn entry(&self) -> Result<&ManifestEntry> {
        self.entry
            .as_ref()
            .map_err(|e| Error::new(e.kind(), e.message().to_owned()))
    }

    /// `<qemu>/<install name>`.
    fn published(&self, entry: &ManifestEntry) -> PathBuf {
        self.dirs.qemu().join(install_name(&entry.qemu_version, &entry.sha256))
    }

    fn install_at(&self, root: &Path, entry: &ManifestEntry, held: Option<Arc<File>>) -> QemuInstall {
        QemuInstall {
            bin: root.join("bin"),
            share: root.join("share"),
            version: entry.qemu_version.clone(),
            zip_sha256: Some(entry.sha256),
            held,
        }
    }

    /// The install at `root`, holding its [`HELD_TOOL`] open; `None` when it is not there. Opening the file by its
    /// published path is the check that the install exists, and from then on the open file keeps it.
    fn holding(&self, root: &Path, entry: &ManifestEntry) -> Result<Option<QemuInstall>> {
        let tool = root
            .join("bin")
            .join(format!("{HELD_TOOL}{}", std::env::consts::EXE_SUFFIX));
        match self.host.open_held_read(&tool, Sharing::Everyone) {
            Ok(file) => Ok(Some(self.install_at(root, entry, Some(Arc::new(file))))),
            Err(e) if crate::host::is_absent(&e) => Ok(None),
            Err(e) => Err(Error::io(ErrorKind::Io, format!("cannot open {}", tool.display()), e)),
        }
    }

    /// The install, downloading and extracting it first if it is not in the cache.
    pub(crate) fn resolve(&self) -> Result<QemuInstall> {
        match &self.config()?.qemu_path {
            Some(dir) => configured(dir, Arch::host()),
            None => self.download(),
        }
    }

    /// The install if it is already there; never downloads.
    pub(crate) fn existing(&self) -> Result<Option<QemuInstall>> {
        if let Some(dir) = &self.config()?.qemu_path {
            return configured(dir, Arch::host()).map(Some);
        }
        let entry = self.entry()?;
        self.holding(&self.published(entry), entry)
    }

    fn download(&self) -> Result<QemuInstall> {
        let entry = self.entry()?;
        let published = self.published(entry);
        if let Some(install) = self.holding(&published, entry)? {
            self.prune_if_idle(&published);
            return Ok(install);
        }
        let zip = self.fetcher.fetch(&entry.artifact())?;

        let qemu = self.dirs.qemu();
        std::fs::create_dir_all(&qemu)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", qemu.display()), e))?;
        let mut lock = FileLock::open(&qemu.join(".extract.lock"))?;
        lock.lock(
            Mode::Exclusive,
            "another devvm process to extract or remove QEMU",
            &mut |m| self.progress.message(m),
        )?;
        self.sweep_extractions(&qemu)?;
        // Another devvm may have published it, or removed it, while this one waited.
        if let Some(install) = self.holding(&published, entry)? {
            return Ok(install);
        }
        self.clear_published(&qemu, &published)?;

        let task = self
            .progress
            .task(format!("extracting QEMU {}", entry.qemu_version), Total::Unknown);
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
        let install = self.install_at(staging.path(), entry, None);
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
        // Under the extraction lock, with the name cleared above: nothing else has taken it since.
        debug_assert!(
            !published.is_dir(),
            "{} was taken under the extraction lock after it was cleared",
            published.display()
        );
        std::fs::rename(staging.path(), &published).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot publish QEMU as {}", published.display()),
                e,
            )
        })?;
        task.finish();
        // Held under the lock, before anything is removed: no other devvm can remove it in between.
        let install = self.holding(&published, entry)?.ok_or_else(|| {
            Error::new(
                ErrorKind::Io,
                format!("{} is gone right after it was published", published.display()),
            )
        })?;
        self.prune_superseded(&qemu, &published);
        Ok(install)
    }

    /// Frees the name `published` for the extraction about to be published there, with the extraction lock held and
    /// the install not found by [`Source::holding`]: whatever is at the name is an install that lost its tool (a
    /// scanner quarantined it, a user deleted it) and is no use. It is renamed to an `.extract-` name and swept, so
    /// nothing is left half-removed under the install's name.
    fn clear_published(&self, qemu: &Path, published: &Path) -> Result<()> {
        let present = crate::host::symlink_metadata_if_present(published)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot look at {}", published.display()), e))?;
        if present.is_none() {
            return Ok(());
        }
        let aside = quarantine_path(qemu, ".extract")?;
        std::fs::rename(published, &aside).map_err(|e| {
            Error::io(
                ErrorKind::Io,
                format!("cannot move the incomplete {} aside", published.display()),
                e,
            )
        })?;
        self.sweep_extractions(qemu)
    }

    /// Under the extraction lock no extraction runs, so every `.extract-*` directory is left over by a killed devvm. One
    /// that cannot be removed (an antivirus holds it) is a warning naming it; a `qemu` that cannot be listed is the error.
    fn sweep_extractions(&self, qemu: &Path) -> Result<()> {
        for warning in crate::host::sweep(qemu, &is_extraction_name, &crate::host::remove_tree)? {
            self.progress.warn(&warning);
        }
        Ok(())
    }

    /// What housekeeping removes from `qemu`: the published installs but `keep`, which are directories named like an
    /// install ([`is_install_name`]), all of the cache that devvm made besides `.extract-` names (a configured
    /// `qemu.path` is never under it), and whether an `.extract-` leftover is there. No install is listed on a host that
    /// cannot tell an install is in use. A listing that fails ends at the failure, with one warning.
    fn surplus(&self, qemu: &Path, keep: &Path) -> Surplus {
        let mut surplus = Surplus {
            installs: Vec::new(),
            extractions: false,
        };
        let listing = match std::fs::read_dir(qemu) {
            Ok(listing) => listing,
            Err(e) => {
                self.progress.warn(&format!("cannot list {}: {e}", qemu.display()));
                return surplus;
            }
        };
        let tells_in_use = !self.host.rename_refused_by_open_file().is_empty();
        for entry in listing {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    self.progress.warn(&format!("cannot list {}: {e}", qemu.display()));
                    break;
                }
            };
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_extraction_name(&name) {
                surplus.extractions = true;
            } else if tells_in_use && path != keep && is_install_name(&name) {
                match crate::host::symlink_metadata_if_present(&path) {
                    Ok(Some(meta)) if meta.is_dir() && !meta.file_type().is_symlink() => surplus.installs.push(path),
                    Ok(_) => {}
                    Err(e) => self.progress.warn(&crate::host::look_failed(&path, e).to_string()),
                }
            }
        }
        surplus
    }

    /// Removes every published install but `keep`, with the extraction lock held.
    ///
    /// An install is first renamed to an `.extract-` name, which nothing resolves. The rename is the test for "in use":
    /// it is refused with one of the host's codes ([`Host::rename_refused_by_open_file`]) while a file is held open
    /// under the directory or the directory is open itself (a working directory), and the install stays as it was,
    /// silently. Any other failure is a warning. Once renamed, the install
    /// is gone as far as every devvm is concerned, and what the delete cannot remove is swept by the next sweep. A
    /// failure on one install does not stop the others.
    fn prune_superseded(&self, qemu: &Path, keep: &Path) {
        let in_use = self.host.rename_refused_by_open_file();
        for path in self.surplus(qemu, keep).installs {
            let removing = match quarantine_path(qemu, ".extract") {
                Ok(removing) => removing,
                Err(e) => {
                    self.progress.warn(e.message());
                    return;
                }
            };
            match std::fs::rename(&path, &removing) {
                Ok(()) => {}
                Err(e) if e.raw_os_error().is_some_and(|code| in_use.contains(&code)) => continue,
                Err(e) => {
                    self.progress
                        .warn(&format!("cannot move {} aside to remove it: {e}", path.display()));
                    continue;
                }
            }
            if let Err(e) = crate::host::remove_tree(&removing) {
                self.progress
                    .warn(&format!("cannot remove {}: {e}", removing.display()));
            }
        }
    }

    /// For a launch that finds its install: removes the installs of other builds and sweeps the leftovers of extractions
    /// and removals that did not finish, but only when there is something to remove and the extraction lock is free. It
    /// never waits: another devvm that holds the lock is extracting or removing, and a later launch finds the work
    /// still there. Housekeeping: a failure is a warning and never fails the launch.
    fn prune_if_idle(&self, published: &Path) {
        let qemu = self.dirs.qemu();
        let surplus = self.surplus(&qemu, published);
        if surplus.installs.is_empty() && !surplus.extractions {
            return;
        }
        let pruned = (|| -> Result<()> {
            let mut lock = FileLock::open(&qemu.join(".extract.lock"))?;
            if lock.try_lock(Mode::Exclusive)? {
                // Every `.extract-` directory is created and renamed from under this lock, so none is in progress.
                self.sweep_extractions(&qemu)?;
                self.prune_superseded(&qemu, published);
            }
            Ok(())
        })();
        if let Err(e) = pruned {
            self.progress.warn(e.message());
        }
    }
}

/// What [`Source::surplus`] found in the cache.
struct Surplus {
    installs: Vec<PathBuf>,
    extractions: bool,
}

/// Whether `name` is an extraction in progress, or what one left: the install of a build being removed has one too.
fn is_extraction_name(name: &str) -> bool {
    name.starts_with(".extract-")
}

/// The directory name of the install of QEMU `version` built into the zip with `sha256`:
/// `<version>-<first 12 hex of the sha256>`. [`is_install_name`] recognises every name this gives.
fn install_name(version: &str, sha256: &Sha256) -> String {
    format!("{version}-{}", &sha256.to_string()[..INSTALL_HASH_LEN])
}

/// Whether `name` is the name of an install ([`install_name`]): something, a `-`, and 12 lowercase hex digits.
fn is_install_name(name: &str) -> bool {
    name.rsplit_once('-').is_some_and(|(version, hash)| {
        !version.is_empty()
            && hash.len() == INSTALL_HASH_LEN
            && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// A QEMU the user installed: `dir` holds the executables, the firmware is in `<dir>/share` or `<dir>/../share`. It must
/// hold what a guest of `arch` needs, the host's in every caller: the only architecture a launch uses today.
fn configured(dir: &Path, arch: Arch) -> Result<QemuInstall> {
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
        held: None,
    };
    let beside = install(dir.join("share"));
    let files = ArchFiles::of(arch);
    require_tools(dir, &["qemu-img", &files.system], ErrorKind::Unavailable)?;
    let firmware = [files.code.as_str(), files.vars];
    let above = dir.parent().map(|parent| parent.join("share"));
    for share in std::iter::once(beside.share.clone()).chain(above.clone()) {
        if firmware_present(&share, &firmware) {
            return Ok(install(share));
        }
    }
    Err(Error::new(
        ErrorKind::Unavailable,
        format!(
            "the configured QEMU in {} has no firmware ({}) in {} or {}",
            dir.display(),
            firmware.join(", "),
            beside.share.display(),
            above.map_or_else(
                || "its parent directory".to_owned(),
                |share| share.display().to_string()
            )
        ),
    ))
}

fn firmware_present(share: &Path, firmware: &[&str]) -> bool {
    firmware.iter().all(|name| share.join(name).is_file())
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
    resolved: Lazy<Arc<QemuInstall>>,
}

impl LazyInstall {
    pub(crate) fn new(
        dirs: Dirs,
        fetcher: Arc<Fetcher>,
        progress: Arc<dyn Progress>,
        host: &'static dyn Host,
    ) -> LazyInstall {
        LazyInstall::from_source(Source::new(
            dirs,
            fetcher,
            progress,
            manifest_entry(crate::TARGET),
            Arc::new(move |install, name| run_version(host, install, name)),
            host,
        ))
    }

    pub(crate) fn from_source(source: Source) -> LazyInstall {
        LazyInstall {
            source,
            resolved: Lazy::new(),
        }
    }

    /// The configuration the install is resolved from, loaded by the first call that succeeds.
    pub(crate) fn config(&self) -> Result<&Config> {
        self.source.config()
    }

    /// The pinned build for this target, or why devvm publishes none.
    pub(crate) fn entry(&self) -> Result<&ManifestEntry> {
        self.source.entry()
    }

    /// The install, resolved once. A failure is not cached.
    pub(crate) fn get(&self) -> Result<Arc<QemuInstall>> {
        self.resolved.get(|| Ok(Arc::new(self.source.resolve()?))).cloned()
    }

    /// The install if it is already there; never downloads, and does not wait for a download in progress.
    pub(crate) fn existing(&self) -> Result<Option<Arc<QemuInstall>>> {
        if let Some(install) = self.resolved.built() {
            return Ok(Some(install.clone()));
        }
        Ok(self.source.existing()?.map(Arc::new))
    }
}
