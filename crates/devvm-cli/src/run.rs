//! The commands that need a VM: `build`, `run`, `up`, `ssh`, `cp`, `start` and `stop`.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use devvm::exit::ExitStatus;
use devvm::recipe::Recipe;
use devvm::{Arch, BuildOptions, Command, Engine, Error, ErrorKind, Guest, Instance, Result, RunAs, StartOptions};

use crate::args::{CpSide, VmFlags};

#[cfg(test)]
#[path = "run_tests.rs"]
mod run_tests;

/// Builds and caches the recipe's layers; no VM is left.
pub(crate) fn build(engine: &Arc<Engine>, reference: &str, arch: Option<Arch>) -> Result<ExitStatus> {
    let recipe = recipe(reference)?;
    engine.build(&recipe, &BuildOptions { arch })?;
    Ok(ExitStatus::Code(0))
}

/// Runs `argv` in an instance that dies with this process, and destroys it when the command is over.
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
        &mut std::io::stderr(),
    );
    drop(vm);
    status
}

/// Creates an instance that stays until `rm` and prints its name.
pub(crate) fn up(engine: &Arc<Engine>, reference: &str, name: &str, flags: &VmFlags) -> Result<ExitStatus> {
    let recipe = recipe(reference)?;
    let image = engine.build(&recipe, &build_options(flags))?;
    let instance = image.up(name, &start_options(flags))?;
    writeln!(std::io::stdout().lock(), "{}", instance.name()).map_err(crate::stdout_error)?;
    Ok(ExitStatus::Code(0))
}

/// What `ssh` was asked for.
enum SshMode {
    Shell,
    Command(Command),
}

/// With a command, the embedded client runs it; without one, the system `ssh` opens a shell, which has no elevated
/// form: `elevated` without a command is a usage error.
fn ssh_mode(elevated: bool, argv: &[String]) -> Result<SshMode> {
    match (argv.is_empty(), elevated) {
        (false, _) => Ok(SshMode::Command(guest_command(argv, elevated))),
        (true, false) => Ok(SshMode::Shell),
        (true, true) => Err(Error::new(
            ErrorKind::Usage,
            "an elevated ssh needs a command: the interactive shell has no elevated form",
        )),
    }
}

pub(crate) fn ssh(engine: &Arc<Engine>, name: &str, elevated: bool, argv: &[String]) -> Result<ExitStatus> {
    let mode = ssh_mode(elevated, argv)?;
    let instance = engine.instance(name)?;
    match mode {
        SshMode::Shell => interactive_ssh(&instance),
        SshMode::Command(command) => {
            instance.run_to(&command, &mut Streaming(std::io::stdout()), &mut std::io::stderr())
        }
    }
}

/// The system `ssh` with the generated configuration, as a child that has this process's console.
fn interactive_ssh(instance: &Instance) -> Result<ExitStatus> {
    let config = instance.ssh_config()?;
    ignore_console_interrupts()?;
    let status = std::process::Command::new("ssh")
        .arg("-F")
        .arg(&config)
        .arg(instance.name())
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

/// Makes Ctrl-C and Ctrl-Break (SIGINT on Unix) do nothing here, so the interactive `ssh` that shares the console alone
/// decides what an interrupt does, and this process then reports its exit code. The handler is installed once and
/// stays; a failed installation is tried again at the next call.
fn ignore_console_interrupts() -> Result<()> {
    static INSTALLED: Mutex<bool> = Mutex::new(false);
    let mut installed = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner);
    if !*installed {
        ctrlc::set_handler(|| {}).map_err(|e| {
            Error::new(
                ErrorKind::Internal,
                format!("cannot make this process ignore Ctrl-C: {e}"),
            )
        })?;
        *installed = true;
    }
    Ok(())
}

/// The `cp` command: one side is a path on this host and the other is `INSTANCE:PATH`.
pub(crate) fn cp(engine: &Arc<Engine>, src: &CpSide, dst: &CpSide) -> Result<ExitStatus> {
    match copy_direction(src, dst)? {
        Transfer::In { instance, from, to } => engine.instance(instance)?.copy_in(from, to)?,
        Transfer::Out { instance, from, to } => engine.instance(instance)?.copy_out(from, to)?,
    }
    Ok(ExitStatus::Code(0))
}

pub(crate) fn start(engine: &Arc<Engine>, name: &str) -> Result<ExitStatus> {
    engine.instance(name)?.start()?;
    Ok(ExitStatus::Code(0))
}

pub(crate) fn stop(engine: &Arc<Engine>, name: &str) -> Result<ExitStatus> {
    engine.instance(name)?.stop()?;
    Ok(ExitStatus::Code(0))
}

// Arguments ===========================================================================================================

fn recipe(reference: &str) -> Result<Recipe> {
    Recipe::from_ref(reference, &crate::current_dir()?)
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

/// Flushes after each write: stdout is line buffered, so a prompt without a newline would otherwise wait for the command
/// to end.
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
