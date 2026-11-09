//! Layer builds: [`Engine::build`] turns a recipe into a chain of cached layers.

use std::io::Write;
use std::sync::Arc;

use super::Engine;
use super::build_dir::BuildDir;
use crate::backend::ShutdownOutcome;
use crate::copy::{self, Snapshot};
use crate::exit::{ExitStatus, Progress};
use crate::guest::{BuildCtx, GuestOs};
use crate::recipe::hash::{CopyManifest, KeyContext, LayerKey, RootContent, chain_keys};
use crate::recipe::{CopyFrom, Flattened, Recipe, Shell, Source, Step};
use crate::store::layers::{BuildRequest, Built, ChainLease, Staging};
use crate::vm::{Image, Vm};
use crate::{Arch, BuildOptions, Command, Error, ErrorKind, Result, RunAs, StartOptions, random};

#[cfg(test)]
#[path = "build_tests.rs"]
mod build_tests;

/// The shell script of a `copy` step's one guest command. `$1` is the destination directory, `$2` the uploaded archive.
/// The archive is removed whatever the extraction did, and the command ends with the extraction's status.
const EXTRACT: &str = r#"mkdir -p -- "$1" && tar -xf "$2" -C "$1"; status=$?; rm -f -- "$2"; exit $status"#;

impl Engine {
    /// Builds the image `recipe` describes: a layer for its base, then one per step. A layer whose inputs match one
    /// already built is reused, so a step runs only when it or an earlier step changed. A `copy` step's layer is
    /// keyed by the files it copies, so touching an untracked file reuses it too.
    ///
    /// A step runs in a transient instance of the layer before it, which `Image::start` gives a lifetime of this
    /// process: it is shut down gracefully, its disk is committed as the step's layer, and the instance is destroyed.
    /// Nothing is committed for a step that fails or whose guest does not shut down, and nothing is published for it.
    /// A guest that was already down when asked (it crashed, or the step powered it off) did not shut down.
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
            let next = store.build(&request(*key), Some(&chain), |staging| step.run(&chain, staging))?;
            chain = next;
        }
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
                    ..
                } => copy::snapshot(
                    self.host,
                    &flat_step.origin_dir,
                    *read_only,
                    into,
                    self.progress.as_ref(),
                )
                .map(Some),
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
    /// Runs the step in a transient instance of `parent` and commits the disk it leaves into `staging`.
    fn run(&self, parent: &ChainLease, staging: &Staging<'_>) -> Result<Built> {
        let engine = self.engine;
        // The image holds a lease of its own on the parent's layers, so that the instance's record names layers that
        // stay.
        let store = engine.store()?;
        let guard = store.begin_record()?;
        let chain = store.lease_chain(&parent.tip().layer_ref(), &guard)?;
        drop(guard);
        let image = Image::from_chain(engine.clone(), chain, self.flat, self.arch);
        let step = &self.flat.steps[self.index].step;
        engine.progress.message(&format!(
            "step {} of {}: {}",
            self.index + 1,
            self.flat.steps.len(),
            describe(step)
        ));
        let vm = image.start(&StartOptions::default())?;
        match step {
            Step::Run {
                command,
                shell,
                run_as,
                cwd,
            } => self.run_command(&vm, command, *shell, *run_as, cwd.clone())?,
            Step::Copy { to, .. } => self.copy(&vm, to)?,
            Step::Set(_) => {
                return Err(Error::new(ErrorKind::Internal, "a set step reached a build"));
            }
        }
        if vm.shutdown()? == ShutdownOutcome::AlreadyDown {
            return Err(Error::new(
                ErrorKind::ConnectionLost,
                format!(
                    "the guest of step {} of {} was down before it was asked to shut down; nothing is committed",
                    self.index + 1,
                    self.flat.steps.len()
                ),
            ));
        }
        engine
            .backend()?
            .storage()
            .commit_layer(&vm.instance().dir().disk(), staging.dir)?;
        Ok(Built::default())
    }

    fn run_command(
        &self,
        vm: &Vm,
        script: &str,
        shell: Option<Shell>,
        run_as: RunAs,
        cwd: Option<String>,
    ) -> Result<()> {
        let command = Command {
            argv: self.guest.shell_argv(shell, script)?,
            run_as,
            cwd,
        };
        self.execute(vm, &command, &format!("`{script}`"))
    }

    /// Uploads the step's snapshot and extracts it into `to`.
    fn copy(&self, vm: &Vm, to: &str) -> Result<()> {
        let snapshot = self
            .snapshot
            .ok_or_else(|| Error::new(ErrorKind::Internal, "a copy step has no snapshot"))?;
        let archive = format!("/tmp/devvm-copy-{}.tar", hex::encode(random::bytes::<16>()));
        let destination = {
            // The op lock is held for the whole upload; the extraction below takes its own.
            let (_op, sftp) = vm.sftp()?;
            // A `~` is the guest's home, which the shell would never expand in a quoted argument: it is resolved here.
            let (expanded, home) = sftp.expanded(to)?;
            let destination = guest_destination(&**self.guest, &expanded, &home)?;
            sftp.copy_in(
                &snapshot.archive,
                &archive,
                self.engine.host,
                self.guest,
                self.engine.progress.as_ref(),
            )?;
            destination
        };
        let command = extract_command(&**self.guest, &destination, &archive)?;
        self.execute(vm, &command, &format!("copy to {to}"))
    }

    /// Runs `command`, reporting its output on the progress channel line by line. A status other than 0 fails the
    /// build, naming the step as `what`.
    fn execute(&self, vm: &Vm, command: &Command, what: &str) -> Result<()> {
        let progress = self.engine.progress.as_ref();
        let status = {
            let (mut out, mut err) = (Lines::new(progress), Lines::new(progress));
            vm.run_to(command, &mut out, &mut err)?
        };
        if status == ExitStatus::Code(0) {
            return Ok(());
        }
        Err(Error::new(
            ErrorKind::Recipe,
            format!(
                "step {} of {} ({what}) ended with {}",
                self.index + 1,
                self.flat.steps.len(),
                status.describe()
            ),
        ))
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

/// A writer that reports each complete line it is given on the progress channel, and the rest when it is dropped.
struct Lines<'a> {
    progress: &'a dyn Progress,
    pending: Vec<u8>,
}

impl<'a> Lines<'a> {
    fn new(progress: &'a dyn Progress) -> Lines<'a> {
        Lines {
            progress,
            pending: Vec::new(),
        }
    }

    fn report(&self, line: &[u8]) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        self.progress.message(&String::from_utf8_lossy(line));
    }
}

impl Write for Lines<'_> {
    fn write(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(chunk);
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            self.report(&line[..end]);
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
