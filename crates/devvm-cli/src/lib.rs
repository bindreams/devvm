//! Command-line interface of devvm: argument parsing, dispatch and the exit contract.
//!
//! The panic hook is installed here and nowhere in the library: a library user's panic unwinds normally.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use clap::{CommandFactory, Parser};
use devvm::exit::{ExitStatus, ExitWidth};
use devvm::progress::verbose_from_env;
use devvm::store::fetch::{LazyHttp, Transport};
use devvm::{Engine, Error, ErrorKind, ListFilter, Result};

mod args;
mod doctor;
#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;
mod output;
mod run;
mod tty;

use args::Cli;
pub use args::{Cmd, CpSide, VmFlags, parse_cp_side};
use output::{Ended, Run};

#[cfg(test)]
fn main() {
    skuld::run_all();
}

// Command line ========================================================================================================

/// What the entry point's collaborators are, injected so a test can place a fault in a real code path.
pub struct Deps {
    pub transport: Arc<dyn Transport>,
}

impl Deps {
    /// Real HTTP, created on first use.
    pub fn real() -> Deps {
        Deps {
            transport: Arc::new(LazyHttp::new()),
        }
    }
}

/// The `--status-file` the arguments or the environment name, found without a full parse so that even a usage error
/// can be reported there. Flags after a `--` belong to the guest command and are ignored; an empty value is no value.
pub fn status_file_hint(args: &[OsString], env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        let text = arg.to_string_lossy();
        if text == "--status-file" {
            match args.next() {
                // The `--` ends the options, so it is no value.
                Some(value) if value == "--" => break,
                Some(value) if !value.is_empty() => return Some(PathBuf::from(value)),
                _ => {}
            }
        } else if let Some(value) = text.strip_prefix("--status-file=")
            && !value.is_empty()
        {
            return Some(PathBuf::from(value));
        }
    }
    env("DEVVM_STATUS_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

// Panic hook ==========================================================================================================

static STATUS_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);

fn set_status_file(path: Option<PathBuf>) {
    *STATUS_FILE.lock().unwrap_or_else(PoisonError::into_inner) = path;
}

/// Makes a panic on any thread print one `devvm-error: panic:` line, write the status file if one is set, and exit 125.
///
/// Exactly one ending prints; see [`output::Run`] for which. The hook never panics: it ignores write errors and
/// tolerates a poisoned lock. A Ctrl-C or a crash (a panic) can leave the last frame of the progress bars on screen
/// above the error line.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let text = payload
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic without a message".to_owned());
        let message = match info.location() {
            Some(location) => format!("{text} (at {}:{})", location.file(), location.line()),
            None => text,
        };
        let status_file = STATUS_FILE.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let ended = output::RUN.on_panic(
            message,
            output::stderr_colour(),
            status_file.as_deref(),
            ExitWidth::Full32,
            &mut std::io::stderr(),
        );
        std::process::exit(ended.code);
    }));
}

// Entry points ========================================================================================================

fn dispatch(deps: &Deps, command: Cmd, verbose: bool) -> Result<ExitStatus> {
    let progress = output::sinks(verbose).progress;
    // `doctor` builds its own engine, one that does not fail on config.yaml; no engine is built twice.
    if let Cmd::Doctor = command {
        return doctor::doctor(deps.transport.clone(), progress);
    }
    let engine = Engine::from_env(progress, deps.transport.clone());
    match command {
        Cmd::Ls { all, json } => list(&engine?, all, json),
        Cmd::Rm { name } => {
            engine?.instance(&name)?.remove()?;
            Ok(ExitStatus::Code(0))
        }
        Cmd::Build { reference, arch } => run::build(&engine?, &reference, arch),
        Cmd::Run {
            reference,
            flags,
            elevated,
            argv,
        } => run::run(&engine?, &reference, &flags, elevated, &argv),
        Cmd::Up { reference, name, flags } => run::up(&engine?, &reference, &name, &flags),
        Cmd::Ssh {
            instance,
            elevated,
            argv,
        } => run::ssh(&engine?, &instance, elevated, &argv),
        Cmd::Cp { src, dst } => run::cp(&engine?, &src, &dst),
        Cmd::Start { name } => run::start(&engine?, &name),
        Cmd::Stop { name } => run::stop(&engine?, &name),
        Cmd::Doctor => unreachable!("handled above"),
    }
}

/// A failed write to stdout.
pub(crate) fn stdout_error(e: std::io::Error) -> Error {
    Error::io(ErrorKind::Io, "cannot write to stdout", e)
}

/// The working directory, which a recipe reference and `ls` are read against.
fn current_dir() -> Result<PathBuf> {
    std::env::current_dir().map_err(|e| Error::io(ErrorKind::Io, "cannot determine the working directory", e))
}

/// `ls`: the instances created from this directory, or all of them.
fn list(engine: &Arc<Engine>, all: bool, json: bool) -> Result<ExitStatus> {
    let filter = if all {
        ListFilter::All
    } else {
        ListFilter::CreatedIn(current_dir()?)
    };
    let infos = engine.list(filter)?;
    let mut out = std::io::stdout().lock();
    let write = |out: &mut std::io::StdoutLock<'_>, text: &str| writeln!(out, "{text}").map_err(stdout_error);
    if json {
        for info in &infos {
            write(&mut out, &info.to_json_line())?;
        }
    } else if !infos.is_empty() {
        let width = infos
            .iter()
            .map(|info| info.name.len())
            .max()
            .unwrap_or(0)
            .max("NAME".len());
        write(
            &mut out,
            &format!("{:width$}  {:8}  {:7}  {:6}  OWNER", "NAME", "STATE", "GUEST", "ARCH"),
        )?;
        for info in &infos {
            write(
                &mut out,
                &format!(
                    "{:width$}  {:8}  {:7}  {:6}  {}",
                    info.name,
                    info.state.as_str(),
                    info.guest.as_str(),
                    info.arch.as_str(),
                    if info.ownerless { "none" } else { "process" },
                ),
            )?;
        }
    }
    Ok(ExitStatus::Code(0))
}

/// Runs the command line `args` (without the program name) and returns the process exit code.
///
/// Installs the panic hook and points it at the status file before anything else runs. An interactive `ssh` leaves
/// console interrupts ignored for the rest of the process. One command runs at a time per process; a concurrent second
/// call is not supported.
pub fn main_entry(width: ExitWidth, args: Vec<OsString>, deps: Deps) -> i32 {
    main_entry_with(width, args, deps, &dispatch)
}

/// `main_entry` with the dispatch replaced; hook installation, status-file setup, parsing and the ending are the real
/// code. The action gets whether the run is verbose (`-v` or `DEVVM_VERBOSE`).
pub fn main_entry_with(
    width: ExitWidth,
    args: Vec<OsString>,
    deps: Deps,
    action: &dyn Fn(&Deps, Cmd, bool) -> Result<ExitStatus>,
) -> i32 {
    install_panic_hook();
    run_ended(&output::RUN, width, args, deps, action).code
}

/// Like [`main_entry`], but ends the process, so that no panic on another thread can print a second line or rewrite
/// the status file between the normal ending and the exit.
pub fn run(width: ExitWidth, args: Vec<OsString>, deps: Deps) -> ! {
    install_panic_hook();
    let ended = run_ended(&output::RUN, width, args, deps, &dispatch);
    std::process::exit(ended.code)
}

/// The command line from status-file setup to the ending, which holds the one-ending lock until `Ended` is dropped. The
/// ending starts once the action is over and `deps` is dropped, not before: a panic in either must end the process by
/// its hook, and a drop may wait on the thread that panics.
fn run_ended(
    run: &Run,
    width: ExitWidth,
    args: Vec<OsString>,
    deps: Deps,
    action: &dyn Fn(&Deps, Cmd, bool) -> Result<ExitStatus>,
) -> Ended {
    let status_file = status_file_hint(&args, &|name| std::env::var_os(name));
    set_status_file(status_file.clone());
    let colour = output::stderr_colour();

    let cli = match Cli::try_parse_from(std::iter::once(OsString::from("devvm")).chain(args)) {
        Ok(cli) => cli,
        Err(e) if !e.use_stderr() => {
            // `--help` and `--version`: printed on stdout, not a failure. A write that fails is one, and only then is
            // the status file written.
            drop(deps);
            let printed = e.print().map_err(stdout_error);
            let (status_file, outcome) = match flushed(printed.map(|()| ExitStatus::Code(0)), &mut std::io::stdout()) {
                Ok(status) => (None, Ok(status)),
                Err(error) => (status_file.as_deref(), Err(error)),
            };
            return run.end(colour, status_file, outcome, width, &mut std::io::stderr());
        }
        Err(e) => {
            drop(deps);
            return run.end(
                colour,
                status_file.as_deref(),
                Err(usage_error(&e)),
                width,
                &mut std::io::stderr(),
            );
        }
    };
    let outcome = action(&deps, cli.command, cli.verbose || verbose_from_env());
    drop(deps);
    let outcome = flushed(outcome, &mut std::io::stdout());
    run.end(colour, status_file.as_deref(), outcome, width, &mut std::io::stderr())
}

/// `outcome` once `out` is flushed. A flush that fails turns a success into an `io` error, since output was lost; a
/// failure that is already the outcome stands.
fn flushed(outcome: Result<ExitStatus>, out: &mut dyn Write) -> Result<ExitStatus> {
    let flush = out.flush().map_err(stdout_error);
    outcome.and_then(|status| flush.map(|()| status))
}

fn usage_error(e: &clap::Error) -> Error {
    let text = e.to_string();
    let first = text.lines().next().unwrap_or("invalid arguments");
    let first = first.strip_prefix("error: ").unwrap_or(first);
    let name = Cli::command().get_name().to_owned();
    Error::new(ErrorKind::Usage, format!("{first} (see '{name} --help')"))
}
