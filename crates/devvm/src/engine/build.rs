//! Layer builds: [`Engine::build`] turns a recipe into a chain of cached layers.

use std::cell::Cell;
use std::io::Write;
use std::sync::Arc;

use super::Engine;
use super::build_dir::BuildDir;
use crate::copy::{self, Snapshot};
use crate::exit::{ExitStatus, Progress};
use crate::guest::{BuildCtx, GuestOs};
use crate::recipe::hash::{CopyManifest, KeyContext, LayerKey, RootContent, chain_keys};
use crate::recipe::{CopyFrom, Flattened, Recipe, Shell, Source, Step};
use crate::store::layers::{BuildRequest, Built, ChainLease, LayerStore, Staging};
use crate::vm::{Image, Vm};
use crate::{Arch, BuildOptions, Command, Error, ErrorKind, Result, RunAs, StartOptions, random};

#[cfg(test)]
#[path = "build_tests.rs"]
mod build_tests;

#[cfg(all(test, windows))]
#[path = "build_windows_tests.rs"]
mod build_windows_tests;

/// The shell script of a `copy` step's one guest command. `$1` is the destination directory, `$2` the uploaded archive.
/// The archive is removed whatever the extraction did, and the command ends with the extraction's status.
const EXTRACT: &str = r#"mkdir -p -- "$1" && tar -xf "$2" -C "$1"; status=$?; rm -f -- "$2"; exit $status"#;

impl Engine {
    /// Builds the image `recipe` describes: a layer for its base, then one per step. A layer whose inputs match one
    /// already built is reused, so a step runs only when it or an earlier step changed. A `copy` step's layer is
    /// keyed by the files it copies, so touching an untracked file reuses it too.
    ///
    /// A step runs in a transient instance of the layer before it, which `Image::start` gives a lifetime of this
    /// process: when the step succeeds the guest flushes its disk (`GuestOs::flush_command`, a command of its own) and
    /// shuts down gracefully, the disk is committed as the step's layer, and the instance is destroyed. Nothing is
    /// committed or published for a step that fails, whose flush fails, or whose guest does not shut down. The flush
    /// runs elevated in a Linux guest, so each step's commit there needs the guest's passwordless `sudo`.
    /// The disk of every instance is the recipe's `resources.disk`, as the keys of the layers say.
    ///
    /// Two builds of one recipe at once run each step once: the second waits for the layer store's build lock, then
    /// finds the layer. A build that is killed leaves its directory in `<state>/builds` for the next
    /// [`Engine::reap`].
    pub fn build(self: &Arc<Engine>, recipe: &Recipe, opts: &BuildOptions) -> Result<Image> {
        let flat = recipe.flatten()?;
        refuse_set_steps(&flat)?;
        let arch = opts.arch.unwrap_or_else(Arch::host);
        let guest = self.guests.os(flat.guest)?;
        let builder = self.guests.builder(flat.guest)?;
        let (root, base) = root_content(&flat.root, arch)?;
        // An engine with no VM backend fails before anything is downloaded or made.
        self.backend()?;
        let store = self.store()?;

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
        let keys = chain_keys(&flat, &ctx, &root, &builder.definition_hash(), &manifests)?;

        let build_ctx = BuildCtx {
            dirs: self.dirs.clone(),
            arch,
            progress: self.progress.clone(),
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
        }
        Ok(Image::from_chain(self.clone(), chain, &flat, arch))
    }

    /// A snapshot of the files each `copy` step takes, in `into`; `None` for the other steps.
    fn snapshots(&self, flat: &Flattened, into: &std::path::Path) -> Result<Vec<Option<Snapshot>>> {
        flat.steps
            .iter()
            .enumerate()
            .map(|(index, flat_step)| match &flat_step.step {
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
                .map_err(|error| {
                    let kind = error.kind();
                    let what = format!("snapshotting the files of copy to {to}");
                    error.with_context(kind, step_of(index, flat.steps.len(), &what))
                }),
                Step::Run { .. } | Step::Set(_) => Ok(None),
            })
            .collect()
    }
}

/// A `set` step has no guest that takes it: no guest implements a setting yet.
fn refuse_set_steps(flat: &Flattened) -> Result<()> {
    match flat
        .steps
        .iter()
        .position(|flat_step| matches!(flat_step.step, Step::Set(_)))
    {
        Some(index) => Err(Error::new(
            ErrorKind::Recipe,
            format!(
                "step {} is `set.uac`, which a {} guest does not have",
                index + 1,
                flat.guest.as_str()
            ),
        )),
        None => Ok(()),
    }
}

/// What the key of the root layer hashes, and the base in the words of an `expired-base` error.
fn root_content(source: &Source, arch: Arch) -> Result<(RootContent, String)> {
    match source {
        Source::DiskImage(pins) => {
            let pin = pins.get(arch).ok_or_else(|| {
                Error::new(
                    ErrorKind::NotSupported,
                    format!("the disk image has no pin for {}", arch.as_str()),
                )
            })?;
            Ok((RootContent::Sha256(pin.sha256), format!("disk image {}", pin.url)))
        }
        Source::Oci { reference, digest } => {
            Ok((RootContent::OciDigest(digest.clone()), format!("OCI image {reference}")))
        }
        Source::Iso { path } => Err(Error::new(
            ErrorKind::NotSupported,
            format!(
                "a layer cannot be keyed by the content of the Windows ISO {} yet",
                path.display()
            ),
        )),
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
    /// Into `flat.steps`.
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
                let kind = error.kind();
                error.with_context(kind, self.step_of("building the step's layer"))
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
        let step = &self.flat.steps[self.index].step;
        engine.progress.message(&format!(
            "step {} of {}: {}",
            self.index + 1,
            self.flat.steps.len(),
            describe(step)
        ));
        let vm = image
            .start(&StartOptions::default())
            .during("starting the step's guest")?;
        match step {
            Step::Run {
                command,
                shell,
                run_as,
                cwd,
            } => self.run_command(&vm, command, *shell, *run_as, cwd.clone())?,
            Step::Copy { to, .. } => self.copy(&vm, to)?,
            Step::Set(_) => {
                return Err(Error::new(ErrorKind::Internal, "a set step reached a build")).during("running the step");
            }
        }
        self.flush(&vm)?;
        vm.shutdown().during("shutting the guest down")?;
        engine
            .backend()
            .during("committing the step's layer")?
            .storage()
            .commit_layer(&vm.instance().dir().disk(), staging.dir)
            .during("committing the step's layer")?;
        Ok(Built::default())
    }

    fn run_command(
        &self,
        vm: &Vm,
        script: &str,
        shell: Option<Shell>,
        run_as: RunAs,
        cwd: Option<String>,
    ) -> StepResult<()> {
        let what = format!("`{script}`");
        let argv = self.guest.shell_argv(shell, script).during(&what)?;
        self.execute(vm, &Command { argv, run_as, cwd }, &what, ErrorKind::Recipe)
    }

    /// Uploads the step's snapshot and extracts it into `to`.
    fn copy(&self, vm: &Vm, to: &str) -> StepResult<()> {
        let snapshot = self
            .snapshot
            .ok_or_else(|| Error::new(ErrorKind::Internal, "a copy step has no snapshot"))
            .during("finding the files of copy")?;
        let archive = format!("/tmp/devvm-copy-{}.tar", hex::encode(random::bytes::<16>()));
        // A `~` is the guest's home, which the shell would never expand in a quoted argument: it is resolved here.
        let resolving = format!("resolving the destination of copy to {to}");
        let destination = vm
            .with_sftp(|sftp| sftp.expanded(to))
            .and_then(|(expanded, home)| guest_destination(&**self.guest, &expanded, &home))
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
        .during(&format!("uploading the files of copy to {to}"))?;
        let what = format!("extracting the files of copy to {to}");
        let command = extract_command(&**self.guest, &destination, &archive).during(&what)?;
        self.execute(vm, &command, &what, ErrorKind::Recipe)
    }

    /// Runs the guest's flush command, which returns once the guest's writes are on its disk. Anything but exit 0 fails
    /// the build: the disk is not committed.
    fn flush(&self, vm: &Vm) -> StepResult<()> {
        self.execute(vm, &self.guest.flush_command(), "the guest's flush", ErrorKind::Io)
    }

    /// Runs `command`, reporting its output on the progress channel line by line. A status other than 0 fails the
    /// build with an error of `kind`, as does a command that cannot be run; both name the step and `what` ran.
    fn execute(&self, vm: &Vm, command: &Command, what: &str, kind: ErrorKind) -> StepResult<()> {
        let progress = self.engine.progress.as_ref();
        let (mut out, mut err) = (Lines::new(progress), Lines::new(progress));
        let status = vm.run_to(command, &mut out, &mut err).during(what)?;
        if status == ExitStatus::Code(0) {
            return Ok(());
        }
        Err(StepFailure::Ended {
            what: what.to_owned(),
            kind,
            status,
        })
    }

    /// `step N of M (what)`.
    fn step_of(&self, what: &str) -> String {
        step_of(self.index, self.flat.steps.len(), what)
    }
}

/// `step N of M (what)` for the step at `index` of `total`.
fn step_of(index: usize, total: usize, what: &str) -> String {
    format!("step {} of {total} ({what})", index + 1)
}

type StepResult<T> = std::result::Result<T, StepFailure>;

/// Why a step failed, with what it was doing. It is the only error the work of a step returns, and nothing converts an
/// [`Error`] into it but [`During::during`], so a step's error cannot leave it without the step and the phase.
enum StepFailure {
    /// `what` failed with `error`.
    Failed { what: String, error: Error },
    /// The command `what` ran ended with `status`, which is not 0.
    Ended {
        what: String,
        kind: ErrorKind,
        status: ExitStatus,
    },
}

impl StepFailure {
    /// The error in the words of `step`: `step N of M (what): <error>`, or `step N of M (what) ended with <status>`.
    fn into_error(self, step: &StepRun<'_>) -> Error {
        match self {
            StepFailure::Failed { what, error } => {
                let kind = error.kind();
                error.with_context(kind, step.step_of(&what))
            }
            StepFailure::Ended { what, kind, status } => Error::new(
                kind,
                format!("{} ended with {}", step.step_of(&what), status.describe()),
            ),
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
        Step::Set(_) => "set".to_owned(),
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

/// The one guest command of a `copy` step: makes `destination`, extracts `archive` into it, removes `archive`
/// whatever the extraction did, and ends with the extraction's status. The paths are arguments of the script, never
/// text in it, so no quoting of theirs is written here: the guest's own wrapping quotes them. The archive is outside
/// `destination` and gone before the disk is committed, so no later layer or instance carries it.
fn extract_command(guest: &dyn GuestOs, destination: &str, archive: &str) -> Result<Command> {
    let mut argv = guest.shell_argv(Some(Shell::Sh), EXTRACT)?;
    argv.extend(["devvm-copy".to_owned(), destination.to_owned(), archive.to_owned()]);
    Ok(Command {
        argv,
        run_as: RunAs::User,
        cwd: None,
    })
}

/// A writer that reports each complete line it is given on the progress channel, and the rest when it is dropped. A
/// line ends at `\n`, at `\r` (a tool that redraws its progress on one line) or at `\r\n`, which is one end however
/// the writes split it.
struct Lines<'a> {
    progress: &'a dyn Progress,
    pending: Vec<u8>,
    /// The last byte written was `\r`, so a `\n` that follows is the other half of its line end.
    after_cr: bool,
}

impl<'a> Lines<'a> {
    fn new(progress: &'a dyn Progress) -> Lines<'a> {
        Lines {
            progress,
            pending: Vec::new(),
            after_cr: false,
        }
    }

    fn report(&self, line: &[u8]) {
        self.progress.message(&String::from_utf8_lossy(line));
    }
}

impl Write for Lines<'_> {
    fn write(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
        for &byte in chunk {
            match byte {
                b'\n' if self.after_cr => {}
                b'\r' | b'\n' => {
                    self.report(&self.pending);
                    self.pending.clear();
                }
                _ => self.pending.push(byte),
            }
            self.after_cr = byte == b'\r';
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
