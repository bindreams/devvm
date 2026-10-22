//! Command-line interface of devvm: argument parsing, dispatch and the exit contract.
//!
//! The panic hook is installed here and nowhere in the library: a library user's panic unwinds normally.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use clap::{CommandFactory, Parser, Subcommand};
use devvm::exit::{ExitStatus, ExitWidth, finish};
use devvm::recipe::Recipe;
use devvm::store::fetch::{HttpTransport, Response, Resume, Transport};
use devvm::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;

#[cfg(test)]
fn main() {
    skuld::run_all();
}

// Command line ========================================================================================================

#[derive(Parser)]
#[command(name = "devvm", version, about = "Disposable virtual machines for tests")]
struct Cli {
    /// Write a JSON description of how the command ended to this file (also: DEVVM_STATUS_FILE).
    #[arg(long, global = true, value_name = "PATH", env = "DEVVM_STATUS_FILE")]
    status_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

/// The commands this build understands.
#[derive(Clone, Debug, PartialEq, Eq, Subcommand)]
pub enum Cmd {
    /// Build the image a recipe describes.
    Build {
        /// A recipe file (`*.devvm.yaml`) or the name of an embedded base recipe.
        reference: String,
    },
}

/// What the entry point's collaborators are, injected so a test can place a fault in a real code path.
pub struct Deps {
    pub transport: Arc<dyn Transport>,
}

impl Deps {
    /// Real HTTP, created on first use.
    pub fn real() -> Deps {
        Deps {
            transport: Arc::new(LazyHttp(OnceLock::new())),
        }
    }
}

struct LazyHttp(OnceLock<std::result::Result<HttpTransport, String>>);

impl Transport for LazyHttp {
    fn get(&self, url: &str, resume: Option<Resume>) -> Result<Response> {
        match self
            .0
            .get_or_init(|| HttpTransport::new().map_err(|e| e.message().to_owned()))
        {
            Ok(transport) => transport.get(url, resume),
            Err(message) => Err(Error::new(ErrorKind::Internal, message.clone())),
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

fn dispatch(_deps: &Deps, command: Cmd) -> Result<ExitStatus> {
    match command {
        Cmd::Build { reference } => {
            let cwd = std::env::current_dir()
                .map_err(|e| Error::io(ErrorKind::Io, "cannot determine the working directory", e))?;
            Recipe::from_ref(&reference, &cwd)?.flatten()?;
            Err(Error::new(
                ErrorKind::NotSupported,
                "build is not wired yet in this build of devvm",
            ))
        }
    }
}

/// Runs the command line `args` (without the program name) and returns the process exit code.
///
/// Installs the panic hook and points it at the status file before anything else runs.
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
            let _ = e.print();
            return Exit {
                code: 0,
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
    let code = finish(status_file.as_deref(), outcome, width, &mut std::io::stderr());
    let _ = std::io::stdout().flush();
    Exit {
        code,
        _only_one_ending: guard,
    }
}

fn usage_error(e: &clap::Error) -> Error {
    let text = e.to_string();
    let first = text.lines().next().unwrap_or("invalid arguments");
    let first = first.strip_prefix("error: ").unwrap_or(first);
    let name = Cli::command().get_name().to_owned();
    Error::new(ErrorKind::Usage, format!("{first} (see '{name} --help')"))
}
