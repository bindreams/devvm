//! Command-line interface of devvm: argument parsing, dispatch and the exit contract.
//!
//! The panic hook is installed here and nowhere in the library: a library user's panic unwinds normally.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use clap::{CommandFactory, Parser};
use devvm::exit::{ExitStatus, ExitWidth, finish};
use devvm::store::fetch::{LazyHttp, Transport};
use devvm::{Engine, Error, ErrorKind, ListFilter, Result};

mod args;
mod doctor;
#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;
mod run;

use args::Cli;
pub use args::{Cmd, CpSide, VmFlags, parse_cp_side};

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
static PANICKING: Mutex<()> = Mutex::new(());

fn set_status_file(path: Option<PathBuf>) {
    *STATUS_FILE.lock().unwrap_or_else(PoisonError::into_inner) = path;
}

/// Makes a panic on any thread print one `devvm-error: panic:` line, write the status file if one is set, and exit 125.
///
/// Concurrent panics queue on a process-wide lock, which the normal finish takes too; the first to get it ends the
/// process, so exactly one line is printed. The hook never panics itself: it ignores write errors and tolerates a poisoned lock.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let _only_one_at_a_time = PANICKING.lock().unwrap_or_else(PoisonError::into_inner);
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
        let code = finish(
            status_file.as_deref(),
            Err(Error::new(ErrorKind::Panic, message)),
            ExitWidth::Full32,
            &mut std::io::stderr(),
        );
        std::process::exit(code);
    }));
}

// Entry points ========================================================================================================

fn dispatch(deps: &Deps, command: Cmd) -> Result<ExitStatus> {
    // `doctor` builds its own engine, one that does not fail on config.yaml; no engine is built twice.
    if let Cmd::Doctor = command {
        return doctor::doctor(deps.transport.clone());
    }
    let engine = Engine::from_env_stderr(deps.transport.clone());
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
    let write = |out: &mut std::io::StdoutLock<'_>, text: &str| {
        writeln!(out, "{text}").map_err(|e| Error::io(ErrorKind::Io, "cannot write to stdout", e))
    };
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
/// console interrupts ignored for the rest of the process.
pub fn main_entry(width: ExitWidth, args: Vec<OsString>, deps: Deps) -> i32 {
    main_entry_with(width, args, deps, &dispatch)
}

/// `main_entry` with the dispatch replaced; hook installation, status-file setup, parsing and `finish` are the real
/// code. Tests use it to put a fault in a real code path.
pub fn main_entry_with(
    width: ExitWidth,
    args: Vec<OsString>,
    deps: Deps,
    action: &dyn Fn(&Deps, Cmd) -> Result<ExitStatus>,
) -> i32 {
    install_panic_hook();
    run_locked(width, args, deps, action).code
}

/// Like [`main_entry`], but ends the process, so that no panic on another thread can print a second line or rewrite
/// the status file between the normal finish and the exit.
pub fn run(width: ExitWidth, args: Vec<OsString>, deps: Deps) -> ! {
    install_panic_hook();
    let exit = run_locked(width, args, deps, &dispatch);
    std::process::exit(exit.code)
}

/// How the command ended, with the panic hook's lock held: the hook cannot run until this is dropped.
struct Exit {
    code: i32,
    _only_one_ending: MutexGuard<'static, ()>,
}

/// The command line from status-file setup to `finish`. The lock is taken once the action is over and `deps` is
/// dropped, not before: a panic in either must still reach the hook, and a drop may wait on the thread that panics.
fn run_locked(
    width: ExitWidth,
    args: Vec<OsString>,
    deps: Deps,
    action: &dyn Fn(&Deps, Cmd) -> Result<ExitStatus>,
) -> Exit {
    let status_file = status_file_hint(&args, &|name| std::env::var_os(name));
    set_status_file(status_file.clone());
    let ending = || PANICKING.lock().unwrap_or_else(PoisonError::into_inner);

    let cli = match Cli::try_parse_from(std::iter::once(OsString::from("devvm")).chain(args)) {
        Ok(cli) => cli,
        Err(e) if !e.use_stderr() => {
            // `--help` and `--version`: printed on stdout, not a failure.
            drop(deps);
            let guard = ending();
            let printed = e
                .print()
                .map(|()| ExitStatus::Code(0))
                .map_err(|source| Error::io(ErrorKind::Io, "cannot write to stdout", source));
            let code = finish(status_file.as_deref(), printed, width, &mut std::io::stderr());
            return Exit {
                code,
                _only_one_ending: guard,
            };
        }
        Err(e) => {
            drop(deps);
            let guard = ending();
            let code = finish(
                status_file.as_deref(),
                Err(usage_error(&e)),
                width,
                &mut std::io::stderr(),
            );
            return Exit {
                code,
                _only_one_ending: guard,
            };
        }
    };
    let outcome = action(&deps, cli.command);
    drop(deps);
    let guard = ending();
    let outcome = flushed(outcome, &mut std::io::stdout());
    let code = finish(status_file.as_deref(), outcome, width, &mut std::io::stderr());
    Exit {
        code,
        _only_one_ending: guard,
    }
}

/// `outcome` once `out` is flushed. A flush that fails turns a success into an `io` error, since output was lost; a
/// failure that is already the outcome stands.
fn flushed(outcome: Result<ExitStatus>, out: &mut dyn Write) -> Result<ExitStatus> {
    let flush = out
        .flush()
        .map_err(|e| Error::io(ErrorKind::Io, "cannot write to stdout", e));
    outcome.and_then(|status| flush.map(|()| status))
}

fn usage_error(e: &clap::Error) -> Error {
    let text = e.to_string();
    let first = text.lines().next().unwrap_or("invalid arguments");
    let first = first.strip_prefix("error: ").unwrap_or(first);
    let name = Cli::command().get_name().to_owned();
    Error::new(ErrorKind::Usage, format!("{first} (see '{name} --help')"))
}
