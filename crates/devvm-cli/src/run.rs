//! The commands that need a VM: `build`, `run`, `up`, `ssh`, `cp`, `start` and `stop`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use devvm::exit::ExitStatus;
use devvm::recipe::Recipe;
use devvm::{
    Arch, BuildOptions, Command, Dirs, Engine, Error, ErrorKind, Guest, InstanceState, ListFilter, Result, RunAs,
    StartOptions,
};

use crate::args::{CpSide, VmFlags};

#[cfg(test)]
#[path = "run_tests.rs"]
mod run_tests;

/// The `build` command: the recipe's layers are built and cached, and no VM is left.
pub(crate) fn build(engine: &Arc<Engine>, reference: &str, arch: Option<Arch>) -> Result<ExitStatus> {
    let recipe = recipe(reference)?;
    engine.build(&recipe, &BuildOptions { arch })?;
    Ok(ExitStatus::Code(0))
}

/// The `run` command: an instance that dies with this process runs `argv`, and is destroyed when the command is over.
pub(crate) fn run(
    engine: &Arc<Engine>,
    reference: &str,
    flags: &VmFlags,
    elevated: bool,
    argv: &[String],
) -> Result<ExitStatus> {
    let recipe = recipe(reference)?;
    let image = engine.build(&recipe, &build_options(flags))?;
    let vm = image.start(&start_options(flags))?;
    let status = vm.run_to(
        &guest_command(argv, elevated),
        &mut Streaming(std::io::stdout()),
        &mut Streaming(std::io::stderr()),
    );
    // The handle's drop destroys the instance, whichever way the command ended.
    drop(vm);
    status
}

/// The `up` command: the instance stays until `rm`; its name is printed.
pub(crate) fn up(engine: &Arc<Engine>, reference: &str, name: &str, flags: &VmFlags) -> Result<ExitStatus> {
    let recipe = recipe(reference)?;
    let image = engine.build(&recipe, &build_options(flags))?;
    let instance = image.up(name, &start_options(flags))?;
    writeln!(std::io::stdout().lock(), "{}", instance.name())
        .map_err(|e| Error::io(ErrorKind::Io, "cannot write to stdout", e))?;
    Ok(ExitStatus::Code(0))
}

/// The `ssh` command: with a command, the embedded client runs it; without one, the system `ssh` opens a shell.
pub(crate) fn ssh(engine: &Arc<Engine>, name: &str, elevated: bool, argv: &[String]) -> Result<ExitStatus> {
    let instance = engine.instance(name)?;
    if argv.is_empty() {
        return interactive_ssh(engine, instance.name());
    }
    instance.run_to(
        &guest_command(argv, elevated),
        &mut Streaming(std::io::stdout()),
        &mut Streaming(std::io::stderr()),
    )
}

/// The system `ssh` with the generated configuration, as a child that has this process's console.
fn interactive_ssh(engine: &Engine, name: &str) -> Result<ExitStatus> {
    // After `stop` the configuration still names the old forwarded port, which another local service may have taken:
    // `ssh` would fail with a host key or connection error instead of saying the instance is stopped.
    require_running(engine, name)?;
    let config = Dirs::from_env()?.ssh().join("config");
    let status = std::process::Command::new("ssh")
        .arg("-F")
        .arg(&config)
        .arg(name)
        .status()
        .map_err(|e| Error::io(ErrorKind::Io, "cannot run `ssh` (is it installed and on PATH?)", e))?;
    match status.code() {
        Some(code) => Ok(ExitStatus::Code(code)),
        None => Err(Error::new(
            ErrorKind::NoExitStatus,
            format!("`ssh` ended without an exit code ({status})"),
        )),
    }
}

/// Fails with `unavailable` unless the VM of instance `name` is found, as the embedded client does.
fn require_running(engine: &Engine, name: &str) -> Result<()> {
    let state = engine
        .list(ListFilter::All)?
        .into_iter()
        .find(|info| info.name == name)
        .map(|info| info.state);
    match state {
        Some(InstanceState::Running) => Ok(()),
        Some(InstanceState::Stopped) => Err(Error::new(
            ErrorKind::Unavailable,
            format!("instance {name} is not running: start it"),
        )),
        // The reason went to the progress channel while listing.
        Some(InstanceState::Unknown) => Err(Error::new(
            ErrorKind::Unavailable,
            format!("cannot tell whether instance {name} is running"),
        )),
        None => Err(Error::new(ErrorKind::NotFound, format!("no instance called {name}"))),
    }
}

/// The `cp` command: one side is a path on this host and the other is `INSTANCE:PATH`.
pub(crate) fn cp(engine: &Arc<Engine>, src: &CpSide, dst: &CpSide) -> Result<ExitStatus> {
    match copy_direction(src, dst)? {
        Transfer::In { instance, from, to } => engine.instance(instance)?.copy_in(from, to)?,
        Transfer::Out { instance, from, to } => engine.instance(instance)?.copy_out(from, to)?,
    }
    Ok(ExitStatus::Code(0))
}

/// The `start` command.
pub(crate) fn start(engine: &Arc<Engine>, name: &str) -> Result<ExitStatus> {
    engine.instance(name)?.start()?;
    Ok(ExitStatus::Code(0))
}

/// The `stop` command.
pub(crate) fn stop(engine: &Arc<Engine>, name: &str) -> Result<ExitStatus> {
    engine.instance(name)?.stop()?;
    Ok(ExitStatus::Code(0))
}

// Arguments ===========================================================================================================

/// The working directory, which a recipe reference and `ls` are read against.
pub(crate) fn current_dir() -> Result<PathBuf> {
    std::env::current_dir().map_err(|e| Error::io(ErrorKind::Io, "cannot determine the working directory", e))
}

fn recipe(reference: &str) -> Result<Recipe> {
    Recipe::from_ref(reference, &current_dir()?)
}

fn build_options(flags: &VmFlags) -> BuildOptions {
    BuildOptions { arch: flags.arch }
}

fn start_options(flags: &VmFlags) -> StartOptions {
    StartOptions {
        cpus: flags.cpus,
        memory: flags.memory,
        network: flags.network,
    }
}

fn guest_command(argv: &[String], elevated: bool) -> Command {
    Command {
        argv: argv.to_vec(),
        run_as: if elevated { RunAs::Elevated } else { RunAs::User },
        cwd: None,
    }
}

/// Which way a `cp` goes, with the instance and the two paths.
#[derive(Debug, PartialEq, Eq)]
enum Transfer<'a> {
    In {
        instance: &'a str,
        from: &'a Path,
        to: &'a str,
    },
    Out {
        instance: &'a str,
        from: &'a str,
        to: &'a Path,
    },
}

fn copy_direction<'a>(src: &'a CpSide, dst: &'a CpSide) -> Result<Transfer<'a>> {
    match (src, dst) {
        (CpSide::Local(from), CpSide::Guest { instance, path }) => Ok(Transfer::In {
            instance,
            from,
            to: path,
        }),
        (CpSide::Guest { instance, path }, CpSide::Local(to)) => Ok(Transfer::Out {
            instance,
            from: path,
            to,
        }),
        (CpSide::Local(_), CpSide::Local(_)) => Err(Error::new(
            ErrorKind::Usage,
            "cp needs one INSTANCE:PATH side, and both sides are local paths",
        )),
        (CpSide::Guest { .. }, CpSide::Guest { .. }) => Err(Error::new(
            ErrorKind::Usage,
            "cp needs one local side, and both sides are INSTANCE:PATH",
        )),
    }
}

// Output ==============================================================================================================

/// Passes each write on and flushes it, so the guest's output arrives as it is written: the standard output is line
/// buffered, and a prompt with no line break would wait for the end of the command.
struct Streaming<W: Write>(W);

impl<W: Write> Write for Streaming<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.0.write(bytes)?;
        self.0.flush()?;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}
