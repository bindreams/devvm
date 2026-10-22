//! The exit contract, through the real binary and the real entry point.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Barrier};

use devvm::exit::{ExitWidth, NullProgress};
use devvm::store::fetch::{Artifact, Fetcher, Response, Resume, Transport};
use devvm::{Error, ErrorKind, Result, Sha256};
use devvm_cli::Deps;
use serde_json::Value;

const HELPER_VAR: &str = "DEVVM_CLI_TEST_HELPER";

fn main() {
    if let Some(name) = std::env::var_os(HELPER_VAR) {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let code = match name.to_str() {
            Some("panic-on-worker") => panic_on_workers(&args, 1),
            Some("panic-on-two-workers") => panic_on_workers(&args, 2),
            other => panic!("unknown helper {other:?}"),
        };
        std::process::exit(code);
    }
    skuld::run_all();
}

// Helpers =============================================================================================================

/// A transport whose `get` waits until `parties` threads are inside it, then panics.
struct PanickingTransport(Arc<Barrier>);

impl Transport for PanickingTransport {
    fn get(&self, _url: &str, _resume: Option<Resume>) -> Result<Response> {
        self.0.wait();
        panic!("injected transport fault");
    }
}

/// Runs the real entry point with an action that fetches through the real `Fetcher` on `parties` worker threads.
/// `args[0]` is the downloads directory; the rest are the CLI arguments.
fn panic_on_workers(args: &[String], parties: usize) -> i32 {
    let downloads = PathBuf::from(&args[0]);
    let cli_args: Vec<OsString> = args[1..].iter().map(OsString::from).collect();
    let deps = Deps {
        transport: Arc::new(PanickingTransport(Arc::new(Barrier::new(parties)))),
    };
    devvm_cli::main_entry_with(ExitWidth::LowByte, cli_args, deps, &|deps, _command| {
        let workers: Vec<_> = (0..parties)
            .map(|i| {
                let fetcher = Fetcher::new(downloads.clone(), deps.transport.clone(), Arc::new(NullProgress));
                let artifact = Artifact {
                    url: format!("https://example.test/{i}"),
                    sha256: Sha256::from_bytes([i as u8; 32]),
                };
                std::thread::spawn(move || fetcher.fetch(&artifact))
            })
            .collect();
        for worker in workers {
            // The panic hook ends the process; reaching here would mean it did not.
            let _ = worker.join();
        }
        Err(Error::new(
            ErrorKind::Internal,
            "the panic hook did not end the process",
        ))
    })
}

fn temp() -> skuld::TempDir {
    let dir = skuld::TempDir::new().unwrap();
    std::fs::create_dir(dir.join("Anna Ж")).unwrap();
    dir
}

fn devvm(cwd: &Path, args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_devvm"));
    command.current_dir(cwd).args(args).env_remove("DEVVM_STATUS_FILE");
    for (key, value) in env {
        command.env(key, value);
    }
    command.stdin(Stdio::null()).output().unwrap()
}

fn status(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn one_error_line(output: &Output, kind: &str) -> String {
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr:?}");
    assert!(stderr.starts_with(&format!("devvm-error: {kind}: ")), "{stderr:?}");
    stderr
}

// Tests ===============================================================================================================

#[skuld::test]
fn a_bad_flag_exits_125_with_one_error_line_and_a_status_file() {
    let dir = temp();
    let file = dir.join("Anna Ж").join("status.json");
    let output = devvm(
        &dir,
        &["--status-file", file.to_str().unwrap(), "build", "--bogus"],
        &[],
    );
    assert_eq!(output.status.code(), Some(125));
    let line = one_error_line(&output, "usage");
    assert!(
        line.contains("--bogus") && line.contains("(see 'devvm --help')"),
        "{line}"
    );
    assert_eq!(status(&file)["devvm"]["kind"], "usage");
}

#[skuld::test]
fn a_missing_recipe_file_exits_125_with_one_error_line_and_a_status_file() {
    let dir = temp();
    let file = dir.join("Anna Ж").join("status.json");
    let output = devvm(
        &dir,
        &["--status-file", file.to_str().unwrap(), "build", "nothere.devvm.yaml"],
        &[],
    );
    assert_eq!(output.status.code(), Some(125));
    let line = one_error_line(&output, "not-found");
    assert!(line.contains("nothere.devvm.yaml"), "{line}");
    let report = status(&file);
    assert_eq!(report["devvm"]["kind"], "not-found");
    assert!(
        report["devvm"]["message"]
            .as_str()
            .unwrap()
            .contains("nothere.devvm.yaml")
    );
}

#[skuld::test]
fn build_resolves_the_recipe_then_reports_that_it_is_not_wired() {
    let dir = temp();
    let output = devvm(&dir, &["build", "ubuntu-24.04"], &[]);
    assert_eq!(output.status.code(), Some(125));
    let line = one_error_line(&output, "not-supported");
    assert!(line.contains("build is not wired yet"), "{line}");
}

#[skuld::test]
fn a_panic_on_a_worker_thread_exits_125_with_one_error_line_and_a_status_file() {
    let dir = temp();
    let file = dir.join("Anna Ж").join("status.json");
    let downloads = dir.join("Anna Ж").join("downloads");
    let output = Command::new(std::env::current_exe().unwrap())
        .env(HELPER_VAR, "panic-on-worker")
        .args([
            downloads.to_str().unwrap(),
            "--status-file",
            file.to_str().unwrap(),
            "build",
            "ubuntu-24.04",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(125));
    let line = one_error_line(&output, "panic");
    assert!(line.contains("injected transport fault"), "{line}");
    let report = status(&file);
    assert_eq!(report["devvm"]["kind"], "panic");
    assert!(
        report["devvm"]["message"]
            .as_str()
            .unwrap()
            .contains("injected transport fault")
    );
}

#[skuld::test]
fn simultaneous_panics_print_one_line() {
    let dir = temp();
    let file = dir.join("Anna Ж").join("status.json");
    let downloads = dir.join("Anna Ж").join("downloads");
    let output = Command::new(std::env::current_exe().unwrap())
        .env(HELPER_VAR, "panic-on-two-workers")
        .args([
            downloads.to_str().unwrap(),
            "--status-file",
            file.to_str().unwrap(),
            "build",
            "ubuntu-24.04",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(125));
    one_error_line(&output, "panic");
    assert_eq!(status(&file)["devvm"]["kind"], "panic");
}

#[skuld::test]
fn status_file_env_var_is_used_without_the_flag() {
    let dir = temp();
    let file = dir.join("Anna Ж").join("from-env.json");
    let output = devvm(&dir, &["build", "--bogus"], &[("DEVVM_STATUS_FILE", &file)]);
    assert_eq!(output.status.code(), Some(125));
    assert_eq!(status(&file)["devvm"]["kind"], "usage");
}

#[skuld::test]
fn help_and_version_exit_0_without_error_lines() {
    let dir = temp();
    for flag in ["--help", "--version"] {
        let output = devvm(&dir, &[flag], &[]);
        assert_eq!(output.status.code(), Some(0), "{flag}");
        assert!(!output.stdout.is_empty(), "{flag}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("devvm-error"),
            "{flag}"
        );
    }
}

#[skuld::test]
fn an_unwritable_status_file_still_exits_125_with_one_line() {
    let dir = temp();
    let file = dir.join("no such dir").join("status.json");
    let output = devvm(
        &dir,
        &["--status-file", file.to_str().unwrap(), "build", "--bogus"],
        &[],
    );
    assert_eq!(output.status.code(), Some(125));
    let line = one_error_line(&output, "status-file");
    assert!(line.contains("original: usage:"), "{line}");
}
