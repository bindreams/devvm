//! Building the root layer of a Windows guest: Windows Setup, then the first-logon script, in as many launches of one
//! disk as the guest restarts.
//!
//! The ISO is installed from a qcow2 overlay that holds only its patched boot image, so Setup starts without a key
//! press and no copy of the ISO is made. The guest answers from a writable USB disk whose scripts install OpenSSH,
//! PowerShell and ssh-broker and write markers to the serial port ([`serial`]). Every guest restart is a VM process
//! exit (`no_reboot`), so the build relaunches, and goes on only while each launch moves forward. A VM process that
//! exits with a failure status ends the build.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use self::serial::{Launches, LogReader, Serial};
use super::WindowsGuest;
use super::expiry::expiry_from;
use super::pins::VIRTIO_WIN;
use super::templates::{self, AnswerInputs, COMPUTER_NAME, TIME_ZONE};
use crate::access::session::humanly;
use crate::backend::qemu::machine::qemu_log;
use crate::backend::{Backend, DeviceProfile, LAYER_DISK, Machine, MachineSpec, Media, MediaFormat, MediaKind, Patch};
use crate::contain::Lifetime;
use crate::guest::{BaseBuilder, BuildCtx, GuestOs};
use crate::host::{Host, is_absent};
use crate::instance::record::InstanceId;
use crate::iso::{HeldIso, InstallIso, read_install_iso};
use crate::media::{SeedFile, read_files, write_cidata_image};
use crate::recipe::Source;
use crate::recipe::hash::LayerKey;
use crate::store::fetch::Fetcher;
use crate::store::layers::{Generation, LayerDir, LayerMeta};
use crate::vm::{DEFAULT_CPUS, DEFAULT_MEMORY};
use crate::{Error, ErrorKind, Network, Result, random};

mod serial;

#[cfg(test)]
#[path = "builder_tests.rs"]
mod builder_tests;

#[cfg(all(test, windows))]
#[path = "builder_windows_tests.rs"]
mod builder_windows_tests;

/// The directory of the build's own files below [`BuildCtx::work`].
const WORK_DIR: &str = "windows-base";
/// The overlay of the ISO, the answer disk and the machine's own files (QEMU's working directory and log), in it.
const OVERLAY: &str = "install.qcow2";
const ANSWER_DISK: &str = "answer.img";
const MACHINE_DIR: &str = "vm";

/// The smallest disk of a base. A qcow2 disk is sparse, so this costs nothing until it is written, and an instance
/// with a larger disk has C: extended at boot.
const MIN_DISK: u64 = 64 << 30;
/// Room left on the answer disk for the logs WinPE copies to it.
const ANSWER_SPARE: u64 = 8 << 20;

/// The phase name the first-logon script starts with: the marker that says Windows Setup is over. A test pins that the
/// script writes it.
const FIRST_LOGON: &str = "first logon";

/// How long each external event of the build may take before the build fails naming it, and how often the serial log is
/// read for progress while the guest runs. A bound ends a wait on the guest, never decides that the guest is done.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bounds {
    /// Windows Setup, from the first launch to the first-logon script's first marker. Measured: 22 to 40 minutes.
    setup: Duration,
    /// The first-logon script, from its first marker to its end. Measured: about 4 minutes.
    first_logon: Duration,
    poll: Duration,
}

impl Default for Bounds {
    fn default() -> Bounds {
        Bounds {
            setup: Duration::from_secs(75 * 60),
            first_logon: Duration::from_secs(30 * 60),
            poll: Duration::from_secs(1),
        }
    }
}

/// Builds a Windows root layer from the ISO of an ISO root. It holds what [`BuildCtx`] does not carry: the downloader
/// of the virtio-win drivers and the backend whose machines it launches.
pub(crate) struct WindowsBuilder {
    fetcher: Arc<Fetcher>,
    backend: Arc<dyn Backend>,
    host: &'static dyn Host,
    bounds: Bounds,
}

impl WindowsBuilder {
    pub(crate) fn new(
        fetcher: Arc<Fetcher>,
        backend: Arc<dyn Backend>,
        host: &'static dyn Host,
        bounds: Bounds,
    ) -> WindowsBuilder {
        WindowsBuilder {
            fetcher,
            backend,
            host,
            bounds,
        }
    }
}

impl BaseBuilder for WindowsBuilder {
    fn build_base(&self, src: &Source, ctx: &BuildCtx, staging: &LayerDir) -> Result<LayerMeta> {
        let Source::Iso { .. } = src else {
            return Err(Error::new(
                ErrorKind::NotSupported,
                "a Windows base is built from an ISO, which the recipe's `from` must name",
            ));
        };
        let root = ctx
            .iso
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .ok_or_else(|| Error::new(ErrorKind::Internal, "the build has no held ISO to install Windows from"))?;
        let evaluation = root.image.is_evaluation();
        let install = read_install_iso(root)?;
        let expires = Build::prepare(self, ctx, staging, &install)?.run(install, evaluation)?;
        // The store owns the key, the generation and the creation time; it reads only the expiry from what a base
        // builder returns.
        Ok(LayerMeta {
            key: LayerKey::from_bytes([0; 16]),
            generation: Generation::from_bytes([0; 16]),
            parent: None,
            created: SystemTime::UNIX_EPOCH,
            expires,
        })
    }

    fn definition_hash(&self) -> [u8; 32] {
        templates::definition_hash()
    }
}

/// What the build is waiting for: the bound that applies, and what an error calls it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Setup,
    FirstLogon,
}

/// A wait that ran past its bound.
struct Stall {
    awaited: &'static str,
    bound: Duration,
}

/// How a launch ended.
enum Exit {
    /// The VM process exited by itself, with this status.
    Exited(ExitStatus),
    /// The VM was killed because [`Stall`]'s bound passed.
    Stalled(Stall),
}

/// How a launch ended, once its markers are read.
enum Launched {
    /// The guest wrote `DEVVM-BASE-DONE` and powered off.
    Finished,
    /// The guest restarted, and made progress.
    Restarted,
}

/// One build of a base: the files it made and what it has seen of the guest.
struct Build<'a> {
    builder: &'a WindowsBuilder,
    ctx: &'a BuildCtx,
    work: PathBuf,
    /// The layer's disk, which every launch boots.
    disk: PathBuf,
    answer: Media,
    virtio: PathBuf,
    /// The overlay of the ISO, attached to the first launch only.
    overlay: PathBuf,
    launches: Launches,
    /// The last `DEVVM-LICENSE=` line.
    license: Option<String>,
    phase: Phase,
    /// When the phase began.
    since: Instant,
    /// Whether any progress marker has been seen: WinPE, which has no serial port, is over once one has.
    saw_marker: bool,
}

impl<'a> Build<'a> {
    /// Makes everything the launches boot: the answer disk, the overlay of the ISO and the layer's disk.
    fn prepare(
        builder: &'a WindowsBuilder,
        ctx: &'a BuildCtx,
        staging: &LayerDir,
        install: &InstallIso,
    ) -> Result<Build<'a>> {
        let work = ctx.work.join(WORK_DIR);
        let machine_dir = work.join(MACHINE_DIR);
        std::fs::create_dir_all(&machine_dir)
            .map_err(|e| Error::io(ErrorKind::Io, format!("cannot create {}", machine_dir.display()), e))?;
        let virtio = builder.fetcher.fetch(&VIRTIO_WIN.artifact())?;

        let storage = builder.backend.storage();
        // QEMU is given the held file's own path: the name the ISO was opened by could be pointed elsewhere meanwhile.
        let backing = install.iso.final_path(builder.host)?;
        let patches: Vec<Patch> = install
            .boot_patches
            .iter()
            .map(|patch| Patch {
                offset: patch.offset,
                bytes: patch.bytes.clone(),
            })
            .collect();
        let overlay = work.join(OVERLAY);
        storage.overlay_with_patches(&backing, &patches, &overlay)?;
        storage.create_blank_disk(
            ctx.resources.disk.map_or(MIN_DISK, |disk| disk.max(MIN_DISK)),
            ctx.arch,
            staging,
        )?;

        let password = random_password();
        let files = templates::answer_disk_files(&AnswerInputs {
            password: &password,
            image_index: install.image.index,
            language: &install.image.language,
            time_zone: TIME_ZONE,
            computer_name: COMPUTER_NAME,
        })?;
        let answer = write_cidata_image(
            builder.host,
            &files,
            MediaKind::UsbDisk,
            false,
            ANSWER_SPARE,
            &work.join(ANSWER_DISK),
        )?;
        Ok(Build {
            builder,
            ctx,
            disk: staging.path().join(LAYER_DISK),
            work,
            answer,
            virtio,
            overlay,
            launches: Launches::default(),
            license: None,
            phase: Phase::Setup,
            since: Instant::now(),
            saw_marker: false,
        })
    }

    /// Launches the guest until it has finished, and returns when the base ends.
    fn run(mut self, install: InstallIso, evaluation: bool) -> Result<Option<SystemTime>> {
        // The ISO is held until QEMU has opened it, which the first `resume` returning says.
        let mut held = Some(install.iso);
        self.since = Instant::now();
        let mut number = 0;
        let log = loop {
            number += 1;
            let log = self.work.join(format!("serial-{number}.log"));
            match self.launch(number, &log, &mut held)? {
                Launched::Finished => break log,
                Launched::Restarted => {}
            }
        };
        self.report_winpe_log_sizes();
        let Some(read) = &self.license else {
            return Err(with_log_tail(
                Error::new(
                    ErrorKind::Unavailable,
                    "the guest finished without a DEVVM-LICENSE line, so when the base expires is not known",
                ),
                "the serial log",
                &log,
            ));
        };
        expiry_from(read, evaluation, self.ctx.clock.now()).map_err(|error| {
            let kind = error.kind();
            error.with_context(kind, "the Windows base's license read")
        })
    }

    /// One launch of the machine, until its VM process exits.
    fn launch(&mut self, number: usize, log: &Path, held: &mut Option<HeldIso>) -> Result<Launched> {
        let spec = self.spec(number, log)?;
        self.ctx.progress.message(&format!("Windows base: launch {number}"));
        let mut machine = self.builder.backend.launch(&spec)?;
        if let Err(error) = machine.resume() {
            let _ = machine.kill();
            return Err(error);
        }
        // The ISO is open in QEMU once `resume` has returned: `qemu_init` opens every drive before the main loop that
        // serves the control socket's greeting starts (QEMU's source order, not a measurement of ours).
        drop(held.take());
        let mut reader = LogReader::new(log);
        let exit = self.wait(&mut *machine, &mut reader);
        // The VM is gone, killed or exited: this removes what it left outside its directory.
        let killed = machine.kill();
        let exit = exit?;
        killed?;

        let rest = reader.poll(true)?;
        self.observe(&rest);
        let serial = reader.whole();
        match exit {
            Exit::Stalled(stall) => {
                let error = Error::new(
                    ErrorKind::Unavailable,
                    format!(
                        "{} did not finish within {}; the guest was stopped",
                        stall.awaited,
                        humanly(stall.bound)
                    ),
                );
                return Err(self.fail(error, log));
            }
            Exit::Exited(status) if !status.success() => {
                let error = Error::new(
                    ErrorKind::Unavailable,
                    format!("the VM process of launch {number} exited with {status}"),
                );
                let error = with_log_tail(error, "QEMU's log", &qemu_log(&self.work.join(MACHINE_DIR)));
                return Err(self.fail(error, log));
            }
            Exit::Exited(_) => {}
        }
        if let Some(why) = &serial.failed {
            let error = Error::new(
                ErrorKind::Unavailable,
                format!("the Windows guest failed to build the base: {why}"),
            );
            return Err(self.fail(error, log));
        }
        if serial.done {
            return Ok(Launched::Finished);
        }
        if let Err(error) = self.launches.note(serial) {
            return Err(self.fail(error, log));
        }
        Ok(Launched::Restarted)
    }

    /// `error` of a launch whose serial log is `log`, with the end of that log and, while no marker has been written,
    /// what WinPE left on the answer disk.
    fn fail(&self, error: Error, log: &Path) -> Error {
        self.with_winpe_logs(with_log_tail(error, "the serial log", log))
    }

    /// The spec of launch `number`: the install media only in the first.
    fn spec(&self, number: usize, serial_log: &Path) -> Result<MachineSpec> {
        let arch = self.ctx.arch;
        let accel = self
            .builder
            .host
            .accelerator(arch)
            .map_err(|unavailable| Error::new(ErrorKind::Unavailable, unavailable.to_string()))?;
        let mut media = Vec::new();
        if number == 1 {
            media.push(Media {
                path: self.overlay.clone(),
                kind: MediaKind::Cdrom,
                boot_index: Some(1),
                readonly: true,
                format: MediaFormat::Qcow2,
            });
        }
        media.push(Media {
            path: self.virtio.clone(),
            kind: MediaKind::Cdrom,
            boot_index: None,
            readonly: true,
            format: MediaFormat::Raw,
        });
        media.push(self.answer.clone());
        Ok(MachineSpec {
            id: InstanceId::random(),
            dir: self.work.join(MACHINE_DIR),
            arch,
            accel,
            cpus: self.ctx.resources.cpus.unwrap_or(DEFAULT_CPUS),
            memory: self.ctx.resources.memory.unwrap_or(DEFAULT_MEMORY),
            network: Network::Nat,
            disk: self.disk.clone(),
            media,
            profile: DeviceProfile {
                no_reboot: true,
                ..WindowsGuest.device_profile()
            },
            firmware: None,
            forwards: Vec::new(),
            serial_log: serial_log.to_owned(),
            lifetime: Lifetime::DiesWithOwner,
        })
    }

    /// Waits until the machine's VM process exits, reporting the guest's progress as it writes it, and returns how it
    /// exited. The wait ends with [`Exit::Stalled`] when the bound of the phase it is in passes: the bound ends the wait
    /// for an event that may never happen, and the error that follows names it. The caller kills the VM.
    fn wait(&mut self, machine: &mut dyn Machine, reader: &mut LogReader) -> Result<Exit> {
        loop {
            // The status is read before anything can reap the process.
            if let Some(status) = machine.try_wait()? {
                return Ok(Exit::Exited(status));
            }
            let written = reader.poll(false)?;
            self.observe(&written);
            if let Some(stall) = self.stall() {
                return Ok(Exit::Stalled(stall));
            }
            std::thread::sleep(self.builder.bounds.poll);
        }
    }

    /// The bound that has passed, if one has.
    fn stall(&self) -> Option<Stall> {
        let (awaited, bound) = match self.phase {
            Phase::Setup => ("Windows Setup", self.builder.bounds.setup),
            Phase::FirstLogon => ("the first-logon script", self.builder.bounds.first_logon),
        };
        (self.since.elapsed() > bound).then_some(Stall { awaited, bound })
    }

    /// Takes in what the guest wrote: reports each marker, moves to the first-logon phase when its script starts, and
    /// keeps the last license read.
    fn observe(&mut self, written: &Serial) {
        for phase in &written.progress {
            self.ctx.progress.message(&format!("Windows base: {phase}"));
            self.saw_marker = true;
            if self.phase == Phase::Setup && phase.starts_with(FIRST_LOGON) {
                self.phase = Phase::FirstLogon;
                self.since = Instant::now();
            }
        }
        if let Some(license) = &written.license {
            self.license = Some(license.clone());
        }
    }

    /// Says how much of the answer disk's spare room the logs WinPE copied to it use: a log that did not fit is lost,
    /// never the build, so this is where the room is found too small.
    fn report_winpe_log_sizes(&self) {
        let message = match self.winpe_logs() {
            Ok(files) => format!(
                "Windows base: WinPE logs use {} of {ANSWER_SPARE} bytes of the answer disk",
                files.iter().map(|file| file.bytes.len()).sum::<usize>()
            ),
            Err(read) => format!(
                "Windows base: cannot read the logs on the answer disk: {}",
                read.message()
            ),
        };
        self.ctx.progress.message(&message);
    }

    /// The logs `winpe.cmd` copied to the answer disk, those that are there.
    fn winpe_logs(&self) -> Result<Vec<SeedFile>> {
        let names = templates::log_files();
        let paths: Vec<&str> = names.iter().map(String::as_str).collect();
        read_files(&self.work.join(ANSWER_DISK), &paths)
    }

    /// `error`, with what WinPE wrote to the answer disk when the guest has not yet written any marker: WinPE has no
    /// serial port, so this is where a failure in it is told.
    fn with_winpe_logs(&self, error: Error) -> Error {
        if self.saw_marker {
            return error;
        }
        let logs = match self.winpe_logs() {
            Ok(files) if files.is_empty() => "\nthe answer disk holds none of WinPE's logs".to_owned(),
            Ok(files) => files
                .iter()
                .map(|file| {
                    format!(
                        "\n{} ({} bytes), last lines:\n{}",
                        file.name,
                        file.bytes.len(),
                        last_lines(&file.bytes)
                    )
                })
                .collect(),
            Err(read) => format!("\nthe logs on the answer disk cannot be read: {}", read.message()),
        };
        Error::new(error.kind(), format!("{}{logs}", error.message()))
    }
}

/// `error`, with the last lines of the log `what` at `path`: the build's directory, and the log with it, is gone when the
/// error is read. A log that is missing or empty adds nothing, and one that cannot be read says so.
fn with_log_tail(error: Error, what: &str, path: &Path) -> Error {
    let tail = match std::fs::read(path) {
        Ok(bytes) => last_lines(&bytes),
        Err(e) if is_absent(&e) => String::new(),
        Err(e) => format!("(cannot be read: {e})"),
    };
    if tail.is_empty() {
        return error;
    }
    Error::new(error.kind(), format!("{}\nthe end of {what}:\n{tail}", error.message()))
}

/// The last 20 lines of a log, with the line ends of a Windows file taken off.
fn last_lines(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

/// The password of the guest's account and its autologon: random for each build and never stored by devvm (the guest
/// accepts SSH keys only). It has an upper-case letter, a lower-case letter, a digit and a symbol, so that no password
/// policy refuses it.
fn random_password() -> String {
    format!("Dv1!{}", hex::encode(random::bytes::<16>()))
}
