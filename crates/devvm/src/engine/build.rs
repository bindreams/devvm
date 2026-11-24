//! Layer builds: [`Engine::build`] turns a recipe into a chain of cached layers.

use std::cell::Cell;
use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};

use super::Engine;
use super::build_dir::BuildDir;
use crate::copy::{self, Snapshot};
use crate::error::LastLines;
use crate::exit::ExitStatus;
use crate::guest::{BuildCtx, GuestOs};
use crate::iso::{IsoMemo, IsoRoot, key_iso};
use crate::progress::{Progress, Total, strip_ansi};
use crate::recipe::hash::{CopyManifest, KeyContext, LayerKey, RootContent, chain_keys};
use crate::recipe::{CopyFrom, FlatStep, Flattened, ProductKey, Recipe, Source, Step};
use crate::store::layers::{BuildRequest, Built, ChainLease, LayerStore, Staging};
use crate::vm::{Image, Vm};
use crate::{Arch, BuildOptions, Command, Error, ErrorKind, Result, StartOptions, random};

#[cfg(test)]
#[path = "build_tests.rs"]
mod build_tests;

#[cfg(all(test, windows))]
#[path = "build_windows_tests.rs"]
mod build_windows_tests;

#[cfg(all(test, unix))]
#[path = "build_unix_tests.rs"]
mod build_unix_tests;

impl Engine {
    /// Builds the image `recipe` describes: a layer for its base, then one per step. A layer whose inputs match one
    /// already built is reused, so a step runs only when it or an earlier step changed. A `copy` step's layer is
    /// keyed by the files it copies, so touching an untracked file reuses it too.
    ///
    /// A step runs in a fresh guest of the layer before it. When the step succeeds the guest flushes its disk
    /// (`GuestOs::flush_command`) and shuts down gracefully, and the disk is committed as the step's layer; nothing is
    /// committed or published for a step that fails, whose flush fails, or whose guest does not shut down. The flush
    /// runs elevated in a Linux guest, so each step's commit there needs the guest's passwordless `sudo`. The disk of
    /// every guest is the recipe's `resources.disk`.
    ///
    /// Two builds of one recipe at once run each step once: the second waits for the layer store's build lock, then
    /// finds the layer. A build that is killed leaves its directory in `<state>/builds` for the next
    /// [`Engine::reap`].
    pub fn build(self: &Arc<Engine>, recipe: &Recipe, opts: &BuildOptions) -> Result<Image> {
        let flat = recipe.flatten()?;
        // The base layer, then one per step; the hashing, the base and each step nest under it.
        let task = self.progress.task(
            format!("building {}", recipe.name),
            Total::Items(1 + flat.steps.len() as u64),
        );
        let arch = opts.arch.unwrap_or_else(Arch::host);
        let guest = self.guests.os(flat.guest)?;
        // Before anything is built or keyed: a setting the guest does not have, or a script its shell cannot carry, is the
        // recipe's mistake.
        refuse_unavailable_commands(&flat, &*guest)?;
        let builder = self.guests.builder(flat.guest)?;
        self.backend()?;
        let store = self.store()?;
        let memo = IsoMemo::new(&self.dirs);
        let (root, base) = root_of(&flat.root, arch, &|iso, edition, product_key| {
            key_iso(self.host, &memo, iso, edition, product_key, self.progress.as_ref())
        })?;

        let build_dir = BuildDir::create(self)?;
        let snapshots = self.snapshots(&flat, build_dir.path())?;
        let manifests: Vec<Option<CopyManifest>> = snapshots
            .iter()
            .map(|snapshot| snapshot.as_ref().map(|snapshot| snapshot.manifest.clone()))
            .collect();
        let ctx = KeyContext {
            arch,
            disk: flat.resources.disk,
            guest_semantics_version: guest.semantics_version(),
        };
        let keys = chain_keys(&flat, &ctx, &root.content(), &builder.definition_hash(), &manifests)?;

        // The ISO of an ISO root stays held through the root layer's build, which the builder that installs from it ends
        // as soon as the guest's disk has opened it; a root layer that is found takes no ISO, and it is freed below.
        let build_ctx = BuildCtx {
            dirs: self.dirs.clone(),
            arch,
            progress: self.progress.clone(),
            clock: self.clock.clone(),
            resources: flat.resources,
            work: build_dir.path().to_owned(),
            iso: Mutex::new(match root {
                BuildRoot::Iso(root) => Some(root),
                BuildRoot::Pinned(_) => None,
            }),
        };
        let request = |key: LayerKey| BuildRequest {
            key,
            source_label: base.clone(),
        };
        let mut chain = store.build(&request(keys[0]), None, |staging| {
            builder
                .build_base(&flat.root, &build_ctx, staging.dir)
                .map(|meta| Built::from_meta(&meta))
        })?;
        // The root layer is found or built, and no step reads the ISO.
        drop(build_ctx.iso.lock().unwrap_or_else(PoisonError::into_inner).take());
        task.set(1);
        for (index, key) in keys[1..].iter().enumerate() {
            let step = StepRun {
                engine: self,
                flat: &flat,
                guest: &guest,
                arch,
                index,
                snapshot: snapshots[index].as_ref(),
            };
            chain = step.build_layer(store, &request(*key), &chain)?;
            task.set(index as u64 + 2);
        }
        task.finish();
        Ok(Image::from_chain(self.clone(), chain, &flat, arch))
    }

    /// A snapshot of the files each `copy` step takes, in `into`; `None` for the other steps.
    fn snapshots(&self, flat: &Flattened, into: &std::path::Path) -> Result<Vec<Option<Snapshot>>> {
        flat.steps
            .iter()
            .map(|flat_step| match &flat_step.step {
                Step::Copy {
                    from: CopyFrom::GitTracked,
                    read_only,
                    to,
                } => copy::snapshot(
                    self.host,
                    &flat_step.origin_dir,
                    *read_only,
                    into,
                    self.progress.as_ref(),
                )
                .map(Some)
                .map_err(|error| in_step(error, flat_step, &format!("snapshotting the files of copy to {to}"))),
                Step::Run { .. } | Step::Set(_) => Ok(None),
            })
            .collect()
    }
}

/// The first step whose guest command the guest cannot be given is an error, in the words of the guest and the step: a
/// `set` step it cannot apply, a `copy` step whose extraction it cannot run, and a step whose command its wrapping
/// refuses (a script its shell cannot carry, a line over the guest's limit). It is the one check of the commands the
/// recipe holds, made before anything is built or keyed, and it builds each command as the step will run it.
fn refuse_unavailable_commands(flat: &Flattened, guest: &dyn GuestOs) -> Result<()> {
    for flat_step in &flat.steps {
        step_command(guest, &flat_step.step, UNKNOWN_COPY_PATHS)
            .and_then(|command| guest.wrap(&command))
            .map_err(|error| in_step(error, flat_step, &describe(&flat_step.step)))?;
    }
    Ok(())
}

/// The destination and the archive of a `copy` step before it runs, which it learns from the guest's home and a random
/// name; a command built from them is only checked.
const UNKNOWN_COPY_PATHS: (&str, &str) = ("/", "/");

/// The one guest command `step` runs, for the check and for the run. A `copy` step's paths are `(destination, archive)`.
fn step_command(guest: &dyn GuestOs, step: &Step, copy_paths: (&str, &str)) -> Result<Command> {
    match step {
        Step::Run {
            command,
            shell,
            run_as,
            cwd,
        } => Ok(Command {
            argv: guest.shell_argv(*shell, command)?,
            run_as: *run_as,
            cwd: cwd.clone(),
        }),
        Step::Copy { .. } => guest.extract_command(copy_paths.0, copy_paths.1),
        Step::Set(setting) => guest.setting_command(setting),
    }
}

/// What the root layer of a build is made from.
enum BuildRoot {
    /// A disk image or an OCI image, which the key hashes as it is.
    Pinned(RootContent),
    /// A Windows ISO, held, with the product key it is installed with.
    Iso(IsoRoot),
}

impl BuildRoot {
    /// What the key of the root layer hashes.
    fn content(&self) -> RootContent {
        match self {
            BuildRoot::Pinned(content) => content.clone(),
            BuildRoot::Iso(root) => {
                let image = root.image();
                RootContent::Iso {
                    sha256: root.sha256(),
                    image_index: image.index,
                    image_name: image.name.clone(),
                    product_key: root.product_key().cloned(),
                }
            }
        }
    }
}

type KeyIso<'a> = dyn Fn(&std::path::Path, Option<&str>, Option<&ProductKey>) -> Result<IsoRoot> + 'a;

/// The root of a build, and the base in the words of an `expired-base` error. `iso_root` gives the content hash of an
/// ISO, the image an edition selects in it and the key that image is installed with, with the ISO held.
fn root_of(source: &Source, arch: Arch, iso_root: &KeyIso) -> Result<(BuildRoot, String)> {
    match source {
        Source::DiskImage(pins) => {
            let pin = pins.get(arch).ok_or_else(|| {
                Error::new(
                    ErrorKind::NotSupported,
                    format!("the disk image has no pin for {}", arch.as_str()),
                )
            })?;
            Ok((
                BuildRoot::Pinned(RootContent::Pinned(pin.hash)),
                format!("disk image {}", pin.url),
            ))
        }
        Source::Oci { reference, digest } => Ok((
            BuildRoot::Pinned(RootContent::OciDigest(digest.clone())),
            format!("OCI image {reference}"),
        )),
        Source::Iso {
            path,
            edition,
            product_key,
        } => {
            let root = iso_root(path, edition.as_deref(), product_key.as_ref())?;
            Ok((BuildRoot::Iso(root), format!("Windows ISO {}", path.display())))
        }
        Source::Recipe(_) | Source::Embedded(_) => Err(Error::new(
            ErrorKind::Internal,
            "a flattened recipe starts from a disk image, an OCI image or an ISO",
        )),
    }
}

/// One step of a build, run when its layer is not in the store.
struct StepRun<'a> {
    engine: &'a Arc<Engine>,
    flat: &'a Flattened,
    guest: &'a Arc<dyn GuestOs>,
    arch: Arch,
    index: usize,
    /// The files of a `copy` step.
    snapshot: Option<&'a Snapshot>,
}

impl StepRun<'_> {
    /// The layer of the step: the one in `store` for `request`, or one `run` builds on top of `parent`. Every error of
    /// the step's build names the step: the store's own (the build lock, the parent, the staging directory, the
    /// layer's metadata, its publication, an expired base) here, and the step's work in `run`.
    fn build_layer(&self, store: &LayerStore, request: &BuildRequest, parent: &ChainLease) -> Result<ChainLease> {
        let named = Cell::new(false);
        store
            .build(request, Some(parent), |staging| {
                self.run(parent, staging).inspect_err(|_| named.set(true))
            })
            .map_err(|error| {
                if named.get() {
                    return error;
                }
                self.in_step(error, "building the step's layer")
            })
    }

    /// Runs the step in a transient instance of `parent` and commits the disk it leaves into `staging`. Every error
    /// names the step and the phase it was in: the work below returns [`StepFailure`], which no bare `?` converts to.
    fn run(&self, parent: &ChainLease, staging: &Staging<'_>) -> Result<Built> {
        self.work(parent, staging).map_err(|failure| failure.into_error(self))
    }

    fn work(&self, parent: &ChainLease, staging: &Staging<'_>) -> StepResult<Built> {
        let engine = self.engine;
        // The image holds a lease of its own on the parent's layers, so that the instance's record names layers that
        // stay.
        let lease = "leasing the layers the step starts from";
        let store = engine.store().during(lease)?;
        let guard = store.begin_record().during(lease)?;
        let chain = store.lease_chain(&parent.tip().layer_ref(), &guard).during(lease)?;
        drop(guard);
        let image = Image::from_chain(engine.clone(), chain, self.flat, self.arch);
        let step = &self.flat_step().step;
        let task = engine.progress.task(step_label(self.flat_step()), Total::Unknown);
        task.status(describe(step));
        let vm = image
            .start(&StartOptions::default())
            .during("starting the step's guest")?;
        match step {
            Step::Run { command, .. } => self.run_command(&vm, step, &format!("`{command}`"))?,
            Step::Copy { to, .. } => self.copy(&vm, step, to)?,
            Step::Set(setting) => self.run_command(&vm, step, &format!("`{}`", setting.name()))?,
        }
        self.flush(&vm)?;
        vm.shutdown().during("shutting the guest down")?;
        engine
            .backend()
            .during("committing the step's layer")?
            .storage()
            .commit_layer(&vm.instance().dir().disk(), staging.dir)
            .during("committing the step's layer")?;
        task.finish();
        Ok(Built::default())
    }

    /// Runs the command of a `run` or `set` step; `what` names it in an error.
    fn run_command(&self, vm: &Vm, step: &Step, what: &str) -> StepResult<()> {
        let command = step_command(&**self.guest, step, UNKNOWN_COPY_PATHS).during(what)?;
        self.execute(vm, &command, what, ErrorKind::Recipe)
    }

    /// Uploads the step's snapshot and extracts it into `to`.
    fn copy(&self, vm: &Vm, step: &Step, to: &str) -> StepResult<()> {
        let snapshot = self
            .snapshot
            .ok_or_else(|| Error::new(ErrorKind::Internal, "a copy step has no snapshot"))
            .during("finding the files of copy")?;
        let archive = format!(
            "{}/devvm-copy-{}.tar",
            self.guest.copy_scratch_dir(),
            hex::encode(random::bytes::<16>())
        );
        // A `~` is the guest's home, which the shell would never expand in a quoted argument: it is resolved here.
        let resolving = format!("resolving the destination of copy to {to}");
        let destination = vm
            .with_sftp(|sftp| sftp.expanded(to))
            .and_then(|(expanded, home)| guest_destination(&**self.guest, &expanded, &home))
            // `home` is the server's spelling (`/C:/Users/user` on a Windows guest); the command takes the user's.
            .map(|destination| self.guest.user_path(&destination))
            .during(&resolving)?;
        // The op lock is held for the whole upload; the extraction below takes its own.
        vm.with_sftp(|sftp| {
            sftp.copy_in(
                &snapshot.archive,
                &archive,
                self.engine.host,
                self.guest,
                self.engine.progress.as_ref(),
            )
        })
        .during(format!("uploading the files of copy to {to}"))?;
        let what = format!("extracting the files of copy to {to}");
        let command = step_command(&**self.guest, step, (&destination, &archive)).during(&what)?;
        self.execute(vm, &command, &what, ErrorKind::Recipe)
    }

    /// Runs the guest's flush command, which returns once the guest's writes are on its disk. Anything but exit 0 fails
    /// the build: the disk is not committed.
    fn flush(&self, vm: &Vm) -> StepResult<()> {
        self.execute(vm, &self.guest.flush_command(), "the guest's flush", ErrorKind::Io)
    }

    /// Runs `command`, reporting its output on the progress channel line by line (a very long line in parts, see
    /// [`Lines`]). A status other than 0 fails the build with an error of `kind`, as does a command that cannot be
    /// run; both name the step and `what` ran, and end with the last lines the command printed.
    fn execute(&self, vm: &Vm, command: &Command, what: &str, kind: ErrorKind) -> StepResult<()> {
        let progress = self.engine.progress.as_ref();
        let tail = Tail::default();
        let (mut out, mut err) = (Lines::new(progress, &tail), Lines::new(progress, &tail));
        let ran = vm.run_to(command, &mut out, &mut err);
        // The last line may have no line end: it is reported when its writer is dropped.
        drop((out, err));
        let status = ran
            .map_err(|error| error.with_tail(STEP_OUTPUT, &tail.text()))
            .during(what)?;
        if status == ExitStatus::Code(0) {
            return Ok(());
        }
        Err(StepFailure::Ended {
            what: what.to_owned(),
            kind,
            status,
            output: tail.text(),
        })
    }

    fn flat_step(&self) -> &FlatStep {
        &self.flat.steps[self.index]
    }

    fn step_of(&self, what: &str) -> String {
        step_of(self.flat_step(), what)
    }

    fn in_step(&self, error: Error, what: &str) -> Error {
        in_step(error, self.flat_step(), what)
    }
}

/// `<recipe>: step N of M`, the step's place in the recipe that declares it.
fn step_label(step: &FlatStep) -> String {
    format!("{}: step {} of {}", step.recipe, step.position + 1, step.count)
}

/// `<recipe>: step N of M (what)`.
fn step_of(step: &FlatStep, what: &str) -> String {
    format!("{} ({what})", step_label(step))
}

/// `error` with `<recipe>: step N of M (what)` in front of its message.
fn in_step(error: Error, step: &FlatStep, what: &str) -> Error {
    let kind = error.kind();
    error.with_context(kind, step_of(step, what))
}

type StepResult<T> = std::result::Result<T, StepFailure>;

/// Why a step failed, with what it was doing. It is the only error the work of a step returns, and nothing converts an
/// [`Error`] into it but [`During::during`], so a step's error cannot leave it without the step and the phase.
enum StepFailure {
    Failed {
        what: String,
        error: Error,
    },
    /// The command `what` ran ended with `status`, which is not 0.
    Ended {
        what: String,
        kind: ErrorKind,
        status: ExitStatus,
        output: String,
    },
}

impl StepFailure {
    /// The error in the words of `step`: `<recipe>: step N of M (what): <error>`, or
    /// `<recipe>: step N of M (what) ended with <status>`.
    fn into_error(self, step: &StepRun<'_>) -> Error {
        match self {
            StepFailure::Failed { what, error } => step.in_step(error, &what),
            StepFailure::Ended {
                what,
                kind,
                status,
                output,
            } => Error::new(
                kind,
                format!("{} ended with {}", step.step_of(&what), status.describe()),
            )
            .with_tail(STEP_OUTPUT, &output),
        }
    }
}

/// Names the phase of a step that a failed call belongs to.
trait During<T> {
    fn during(self, what: impl Into<String>) -> StepResult<T>;
}

impl<T> During<T> for Result<T> {
    fn during(self, what: impl Into<String>) -> StepResult<T> {
        self.map_err(|error| StepFailure::Failed {
            what: what.into(),
            error,
        })
    }
}

/// The step in the words of a progress line.
fn describe(step: &Step) -> String {
    match step {
        Step::Run { command, .. } => command.lines().next().unwrap_or_default().to_owned(),
        Step::Copy { to, .. } => format!("copy to {to}"),
        Step::Set(setting) => setting.name().to_owned(),
    }
}

/// The guest path `expanded` (a destination with its `~` expanded, `Sftp::expanded`) as an absolute path: a path with
/// no root of the guest's is below `home` (the directory a login shell starts in).
fn guest_destination(guest: &dyn GuestOs, expanded: &str, home: &str) -> Result<String> {
    let (root, _) = guest.split_root(expanded)?;
    if root.is_empty() {
        return Ok(format!("{}/{expanded}", home.trim_end_matches('/')));
    }
    Ok(expanded.to_owned())
}

/// What an error says the tail of a step's output is.
const STEP_OUTPUT: &str = "the step's output";

/// The last lines a step's command printed on either stream, for the error of a step that fails.
#[derive(Default)]
struct Tail(Mutex<LastLines>);

impl Tail {
    fn push(&self, line: &str) {
        let mut lines = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        lines.push(strip_ansi(line).into_owned());
    }

    fn text(&self) -> String {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).text()
    }
}

/// The most bytes of one line that [`Lines`] holds back: 64 KiB, far above the lines tools print for people. It
/// protects the host's memory against a stream with no line end. A line longer than this is reported in parts, each
/// its own output event and its own slot of the [`Tail`], and an escape sequence that straddles a cut leaves its
/// remainder as text. A stored part is lossy text, up to three bytes per input byte, so the [`Tail`]'s `TAIL_LINES`
/// slots hold at most 20 x 192 KiB = 3.75 MiB, and each writer at most one chunk more.
const LINE_CHUNK: usize = 64 * 1024;

/// A writer that reports each complete line as step output and keeps it in a [`Tail`]; the rest goes out when it is
/// dropped. A line ends at `\n`, `\r` or `\r\n` (a tool redrawing its progress). A line longer than [`LINE_CHUNK`] is
/// reported in parts.
struct Lines<'a> {
    progress: &'a dyn Progress,
    tail: &'a Tail,
    pending: Vec<u8>,
    carriage: Carriage,
}

/// What the last byte written says about the `\r` before a `\n`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Carriage {
    None,
    /// The last byte was a `\r` that ended a line, so a `\n` that follows is the other half of its line end.
    EndedLine,
    /// The last byte was a `\r` with no line to end: a `\n` that follows ends an empty line (`\r\n`), and any other
    /// byte makes it the start of a redraw.
    Bare,
}

impl<'a> Lines<'a> {
    fn new(progress: &'a dyn Progress, tail: &'a Tail) -> Lines<'a> {
        Lines {
            progress,
            tail,
            pending: Vec::new(),
            carriage: Carriage::None,
        }
    }

    fn report(&self, line: &[u8]) {
        let line = String::from_utf8_lossy(line);
        self.progress.output(&line);
        self.tail.push(&line);
    }

    /// Reports the pending bytes as one part of a long line, except the start of a character that is cut off at their
    /// end: that stays pending, as bytes, to be completed by the next ones.
    fn flush_part(&mut self) {
        let whole = self.pending.len() - unfinished_character(&self.pending);
        self.report(&self.pending[..whole]);
        self.pending.drain(..whole);
    }
}

/// How many bytes at the end of `bytes` are the start of a UTF-8 character that more bytes would complete.
fn unfinished_character(bytes: &[u8]) -> usize {
    let Some(chunk) = bytes.utf8_chunks().last() else {
        return 0;
    };
    let invalid = chunk.invalid();
    match std::str::from_utf8(invalid) {
        Err(error) if error.error_len().is_none() => invalid.len(),
        _ => 0,
    }
}

impl Write for Lines<'_> {
    fn write(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
        for &byte in chunk {
            match (byte, self.carriage) {
                (b'\n', Carriage::EndedLine) => self.carriage = Carriage::None,
                (b'\n', _) => {
                    self.report(&self.pending);
                    self.pending.clear();
                    self.carriage = Carriage::None;
                }
                // After a `\r` that ended a line, this one is the first half of `\r\r\n`: still that line's end.
                (b'\r', Carriage::EndedLine) => {}
                (b'\r', _) if self.pending.is_empty() => self.carriage = Carriage::Bare,
                (b'\r', _) => {
                    self.report(&self.pending);
                    self.pending.clear();
                    self.carriage = Carriage::EndedLine;
                }
                _ => {
                    // A byte past the chunk is what makes it a part: a line of exactly a chunk stays one line. A part
                    // is never empty, so the line end after it reports nothing extra.
                    debug_assert!(self.pending.len() <= LINE_CHUNK);
                    if self.pending.len() == LINE_CHUNK {
                        self.flush_part();
                    }
                    self.pending.push(byte);
                    self.carriage = Carriage::None;
                }
            }
        }
        Ok(chunk.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Lines<'_> {
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            self.report(&self.pending);
        }
    }
}
