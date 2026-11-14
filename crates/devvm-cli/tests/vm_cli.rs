//! The end-to-end VM suites through the real `devvm` executable: guests that boot under QEMU, reached by `up`, `run`,
//! `ssh`, `cp`, `stop`, `start` and `rm`, and `doctor` on a host that has them.
//!
//! Every test joins the `vm_group` test group (see `test_groups/shared.rs`).

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use devvm::{ExitStatus, host_exit_width};
use serde_json::Value;

use cases::{Case, Ended, ExecTarget, Observed, Os};
use test_groups::{Group, WINDOWS_ISO_VAR, vm_group, windows_guest};
use test_harness::{GUEST, VM};

#[path = "../../devvm/src/test_groups/shared.rs"]
mod test_groups;
#[path = "../../devvm/src/test_harness.rs"]
#[allow(dead_code, reason = "the labels of the groups that this binary does not run")]
mod test_harness;
#[path = "support/unicode.rs"]
mod unicode;
use unicode::UNICODE_DIR;
#[path = "support/home.rs"]
mod home;
#[path = "support/isolate.rs"]
mod isolate;
#[cfg(unix)]
#[path = "support/socket_path_unix.rs"]
mod socket_path;
#[cfg(windows)]
#[path = "support/socket_path_windows.rs"]
mod socket_path;
use home::{Home, stdout_lines};
use socket_path::SOCKET_PATH_MAX;
#[path = "../../devvm/src/test_support/roots.rs"]
mod roots;
use roots::XdgRoots;
#[path = "../../devvm/src/vm_suite/exec/cases.rs"]
mod cases;
#[path = "../../devvm/src/vm_suite/windows/shared.rs"]
mod windows_shared;
use windows_shared::{first_try, recipe_text};

skuld::default_labels!(VM);

fn main() {
    skuld::run_all();
}

// The devvm executable ================================================================================================

/// How to run the `devvm` executable against this process's roots: the environment ([`Home`], which sets all five XDG
/// variables, so that no command reads or sweeps the user's own directories), and a project directory to run it in.
#[derive(Clone)]
struct Cli {
    home: Arc<Home>,
    project: PathBuf,
    scratch: PathBuf,
}

impl Cli {
    /// The command `devvm <args>` in the project directory, with no stdin.
    fn command(&self, args: &[&str]) -> Command {
        self.home.command(&self.project, args)
    }

    fn devvm(&self, args: &[&str]) -> Output {
        self.home.devvm(&self.project, args)
    }

    /// `devvm <args>`, which must exit 0; its stdout.
    fn devvm_ok(&self, args: &[&str]) -> String {
        let output = self.devvm(args);
        assert!(
            output.status.success(),
            "devvm {args:?} ended with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("UTF-8 output")
    }

    /// The directory the environment variable `var` names in the commands of [`Cli::command`].
    fn root_of(&self, var: &str) -> PathBuf {
        self.command(&[])
            .get_envs()
            .find(|(name, _)| *name == var)
            .and_then(|(_, value)| value)
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("{var} is not set for devvm"))
    }

    /// A path in the scratch directory, which is below the roots and goes with them.
    fn scratch(&self, name: &str) -> PathBuf {
        self.scratch.join(name)
    }
}

/// The short directory that is `XDG_RUNTIME_DIR` in these tests, made if need be: the roots' own directories are too
/// long for a control socket path. It is [`roots::socket_dir`] for this host's limit ([`SOCKET_PATH_MAX`], computed as
/// `Host::socket_path_max` computes it), as the lib asks for it, so both pick the same directory.
fn run_dir() -> Result<PathBuf, String> {
    let dir = roots::socket_dir(SOCKET_PATH_MAX).ok_or_else(|| {
        format!(
            "the temporary directory {} is too long for a control socket",
            std::env::temp_dir().display()
        )
    })?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    Ok(dir)
}

/// This process's roots and how to run devvm in them.
struct CliEnv {
    _roots: XdgRoots,
    cli: Cli,
}

#[skuld::fixture(scope = process)]
fn cli_env(#[fixture(vm_group)] _group: &Group) -> Result<CliEnv, String> {
    let run = run_dir()?;
    let roots = XdgRoots::new(Some(&run))?;
    // The cache is shared by the test processes of this machine, so the QEMU build and the Ubuntu image are fetched
    // once.
    let cache = roots.var("XDG_CACHE_HOME").expect("a cache root");
    let project = roots.top.join("project");
    let scratch = roots.top.join("scratch");
    for dir in [&project, &scratch] {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    }
    let cli = Cli {
        home: Arc::new(Home::at(&roots.top, cache, &run)),
        project,
        scratch,
    };
    Ok(CliEnv { _roots: roots, cli })
}

/// What a test that boots a guest takes: it carries the `GUEST` label and the lock that serialises such tests, so that
/// guests boot one at a time on a 4 vCPU runner.
#[skuld::fixture(scope = process, serial = GUEST, labels = [GUEST])]
fn cli_guests(#[fixture(cli_env)] env: &CliEnv) -> Result<Cli, String> {
    Ok(env.cli.clone())
}

/// An instance made by `devvm up`, removed with `devvm rm` when this is dropped, so a failed test leaves no machine.
struct Instance {
    cli: Cli,
    name: String,
    /// The OS of its guest, which the exec table takes its programs from.
    os: Os,
}

impl Instance {
    /// `devvm up ubuntu-24.04 --name <name> <flags>`, which prints the name.
    fn up(cli: &Cli, name: &str, flags: &[&str]) -> Result<Instance, String> {
        Instance::up_from(cli, "ubuntu-24.04", Os::Linux, name, flags)
    }

    /// `devvm up <reference> --name <name> <flags>`, for a guest of `os`.
    fn up_from(cli: &Cli, reference: &str, os: Os, name: &str, flags: &[&str]) -> Result<Instance, String> {
        let mut args = vec!["up", reference, "--name", name];
        args.extend(flags);
        let output = cli.devvm(&args);
        // Made, or half made: either way `rm` removes it.
        let instance = Instance {
            cli: cli.clone(),
            name: name.to_owned(),
            os,
        };
        if !output.status.success() || output.stdout != format!("{name}\n").as_bytes() {
            return Err(format!(
                "devvm {args:?} ended with {}: stdout {:?}, stderr {}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(instance)
    }

    fn devvm_ok(&self, args: &[&str]) -> String {
        self.cli.devvm_ok(args)
    }

    /// `devvm ssh <name> -- <argv>`, which must exit 0; its stdout.
    fn ssh_ok(&self, argv: &[&str]) -> String {
        let mut args = vec!["ssh", self.name.as_str(), "--"];
        args.extend(argv);
        self.cli.devvm_ok(&args)
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // After a `devvm rm` the instance is gone and `rm` fails with `not-found` for this instance, which is not
        // reported: the test has checked what it needed. Nothing here panics: that would abort a test that is already
        // unwinding.
        match self.cli.command(&["rm", &self.name]).output() {
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let gone = format!("devvm-error: not-found: no instance called {}", self.name);
                if !output.status.success() && !stderr.contains(&gone) {
                    eprintln!("devvm rm {} ended with {}: {stderr}", self.name, output.status);
                }
            }
            Err(e) => eprintln!("cannot run devvm rm {}: {e}", self.name),
        }
    }
}

/// An instance that every test of the exec table runs its commands in: one processor and 1 GiB, which stays up for the
/// process.
#[skuld::fixture(scope = process, serial = GUEST, labels = [GUEST])]
fn shared_instance(#[fixture(cli_guests)] cli: &Cli) -> Result<Instance, String> {
    Instance::up(cli, "cli-shared", &["--cpus", "1", "--memory", "1GiB"])
}

/// An instance of a Windows guest built from the ISO `DEVVM_WINDOWS_ISO` names, which stays up for the process. It builds
/// its own base: this binary's stores are not the lib's.
#[skuld::fixture(scope = process, serial = GUEST, labels = [GUEST])]
fn windows_instance(
    #[fixture(cli_guests)] cli: &Cli,
    #[fixture(windows_guest)] _group: &Group,
) -> Result<Instance, String> {
    static FAILED: OnceLock<String> = OnceLock::new();
    first_try(&FAILED, || windows_up(cli))
}

fn windows_up(cli: &Cli) -> Result<Instance, String> {
    let iso = std::env::var_os(WINDOWS_ISO_VAR).ok_or_else(|| format!("{WINDOWS_ISO_VAR} is not set"))?;
    let recipe = cli.project.join("windows.devvm.yaml");
    std::fs::write(&recipe, recipe_text(Path::new(&iso), &[])?)
        .map_err(|e| format!("cannot write {}: {e}", recipe.display()))?;
    Instance::up_from(
        cli,
        "windows.devvm.yaml",
        Os::Windows,
        "cli-windows",
        &["--cpus", "2", "--memory", "4GiB"],
    )
}

// The exec suite ======================================================================================================

/// Runs a case with `devvm ssh <name> -- <argv>`, and reads how it ended from the status file and the exit code, which
/// must agree.
struct ThroughSsh<'a>(&'a Instance);

static STATUS_FILES: AtomicUsize = AtomicUsize::new(0);

impl ExecTarget for ThroughSsh<'_> {
    fn os(&self) -> Os {
        self.0.os
    }

    fn run(&self, case: &Case) -> Observed {
        assert!(case.cwd.is_none(), "devvm has no flag for a working directory");
        let status_file = self
            .0
            .cli
            .scratch(&format!("status-{}.json", STATUS_FILES.fetch_add(1, Ordering::SeqCst)));
        let status_arg = status_file.to_str().expect("a UTF-8 path");
        let mut args = vec!["--status-file", status_arg, "ssh", self.0.name.as_str()];
        if case.elevated {
            args.push("--elevated");
        }
        args.push("--");
        args.extend(case.argv.iter().map(String::as_str));
        let output = self.0.cli.devvm(&args);

        let report: Value = serde_json::from_slice(&std::fs::read(&status_file).unwrap_or_else(|e| {
            panic!(
                "devvm wrote no status file for {:?} ({e}): {}",
                case.argv,
                String::from_utf8_lossy(&output.stderr)
            )
        }))
        .expect("a JSON status file");
        let (ended, status) = match (report.get("guest"), report.get("devvm")) {
            (Some(guest), _) => match (guest.get("code"), guest.get("signal")) {
                (Some(code), _) => {
                    let code = i32::try_from(code.as_i64().expect("an integer code")).expect("a 32-bit code");
                    (Ended::Code(code), ExitStatus::Code(code))
                }
                (_, Some(signal)) => {
                    let name = signal.as_str().expect("a signal name").to_owned();
                    let core_dumped = guest["core_dumped"].as_bool().expect("a core_dumped flag");
                    (Ended::Signal(name.clone()), ExitStatus::Signal { name, core_dumped })
                }
                _ => panic!("a guest report with neither a code nor a signal: {report}"),
            },
            (_, Some(error)) => panic!(
                "devvm failed {:?}: {error}: {}",
                case.argv,
                String::from_utf8_lossy(&output.stderr)
            ),
            _ => panic!("a status file that says neither guest nor devvm: {report}"),
        };
        assert_eq!(
            output.status.code(),
            Some(host_exit_width().exit_code(&status)),
            "the exit code does not match {report}"
        );
        Observed {
            ended,
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

// The cases that need a working directory (`cwd_is_honored`, `elevated_cwd_is_honored`) are the library's alone: the
// command line has no flag for one.

#[skuld::test]
fn cli_exit_7_125_255(#[fixture(shared_instance)] instance: &Instance) {
    cases::exit_7_125_255(&ThroughSsh(instance));
}

#[skuld::test]
fn cli_a_guest_printing_devvm_error_and_exiting_125_is_a_guest_status(#[fixture(shared_instance)] instance: &Instance) {
    cases::a_guest_printing_devvm_error_and_exiting_125_is_a_guest_status(&ThroughSsh(instance));
}

#[skuld::test]
fn cli_kill_minus_segv_dollar_dollar_exits_139(#[fixture(shared_instance)] instance: &Instance) {
    cases::kill_minus_segv_dollar_dollar_exits_139(&ThroughSsh(instance));
}

#[skuld::test]
fn cli_argv_round_trips_hostile_arguments(#[fixture(shared_instance)] instance: &Instance) {
    cases::argv_round_trips_hostile_arguments(&ThroughSsh(instance));
}

#[skuld::test]
fn cli_elevated_runs_as_root(#[fixture(shared_instance)] instance: &Instance) {
    cases::elevated_runs_as_root(&ThroughSsh(instance));
}

// The same table against a Windows guest.

#[skuld::test]
fn cli_windows_exit_codes_7_125_255_and_ntstatus(#[fixture(windows_instance)] instance: &Instance) {
    cases::exit_7_125_255(&ThroughSsh(instance));
}

#[skuld::test]
fn cli_windows_a_guest_printing_devvm_error_and_exiting_125_is_a_guest_status(
    #[fixture(windows_instance)] instance: &Instance,
) {
    cases::a_guest_printing_devvm_error_and_exiting_125_is_a_guest_status(&ThroughSsh(instance));
}

#[skuld::test]
fn cli_windows_argv_round_trips_hostile_arguments(#[fixture(windows_instance)] instance: &Instance) {
    cases::argv_round_trips_hostile_arguments(&ThroughSsh(instance));
}

#[skuld::test]
fn cli_windows_elevated_runs_at_high_integrity(#[fixture(windows_instance)] instance: &Instance) {
    cases::elevated_runs_as_root(&ThroughSsh(instance));
}

/// A reader that goes away stops the command that writes to it: devvm ends the guest command, exits 125 with an `io`
/// error, and does not wait for a status that may never come. The guest's `yes` then ends by its own SIGPIPE or SIGHUP,
/// which the last step waits for.
///
/// It runs `devvm ssh` on an instance, not `devvm run`: the VM of a `run` is destroyed with it, so there would be no
/// guest left to look at.
#[skuld::test]
fn cli_run_into_a_closed_reader_stops_the_guest_command_and_exits_125_without_hanging(
    #[fixture(shared_instance)] instance: &Instance,
) {
    let stderr_file = instance.cli.scratch("closed-reader.stderr");
    let stderr = std::fs::File::create(&stderr_file).unwrap();
    let mut child = instance
        .cli
        .command(&["ssh", &instance.name, "--", "yes"])
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .expect("devvm starts");
    let mut reader = std::io::BufReader::new(child.stdout.take().expect("a piped stdout"));
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, "y\n", "the first line of `yes`");
    drop(reader);

    let status = child.wait().unwrap();

    let stderr = std::fs::read_to_string(&stderr_file).unwrap();
    assert_eq!(status.code(), Some(125), "{stderr}");
    assert!(stderr.contains("devvm-error: io: "), "{stderr}");
    // Waits for the real condition, that no `yes` is left in the guest, and has no bound of its own.
    instance.ssh_ok(&["sh", "-c", "while pgrep -x yes > /dev/null; do sleep 0.2; done"]);
}

// Commands ============================================================================================================

/// `run` boots a VM of its own, runs the command, and prints and exits as the command did; nothing of it is left.
#[skuld::test]
fn cli_run_prints_the_commands_output_and_exits_with_its_status_and_leaves_no_instance(
    #[fixture(cli_guests)] cli: &Cli,
) {
    let output = cli.devvm(&[
        "run",
        "ubuntu-24.04",
        "--cpus",
        "1",
        "--memory",
        "1GiB",
        "--",
        "sh",
        "-c",
        "echo hello; exit 3",
    ]);

    assert_eq!(output.stdout, b"hello\n", "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listed = cli.devvm_ok(&["ls", "--all", "--json"]);
    for line in listed.lines() {
        let name = serde_json::from_str::<Value>(line).unwrap()["name"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(!name.starts_with("run-"), "`run` left the instance {name}");
    }
}

/// Every file below `dir`, `/`-separated, with its content; a directory is a name with no content.
fn read_tree(dir: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn visit(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
            if entry.file_type().unwrap().is_dir() {
                out.insert(name.clone(), None);
                visit(&entry.path(), &format!("{name}/"), out);
            } else {
                out.insert(name, Some(std::fs::read(entry.path()).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(dir, "", &mut out);
    out
}

#[skuld::test]
fn up_ssh_true_cp_both_ways_stop_start_rm(#[fixture(cli_guests)] cli: &Cli) {
    let instance = Instance::up(cli, "cli-life", &["--cpus", "1", "--memory", "1GiB"]).unwrap();
    instance.ssh_ok(&["true"]);

    // A file and a tree with a space and Ж in their names, to `~/` and back.
    let file = cli.scratch("with space \u{416}.txt");
    std::fs::write(&file, b"a file").unwrap();
    let tree = cli.scratch("tree");
    std::fs::create_dir_all(tree.join("sub dir")).unwrap();
    std::fs::write(tree.join("sub dir").join("\u{416} nested.txt"), b"nested").unwrap();
    std::fs::write(tree.join("top.txt"), b"top").unwrap();
    let into = |local: &Path, guest: &str| {
        instance.devvm_ok(&["cp", local.to_str().unwrap(), &format!("cli-life:{guest}")]);
    };
    let out_of = |guest: &str, local: &Path| {
        instance.devvm_ok(&["cp", &format!("cli-life:{guest}"), local.to_str().unwrap()]);
    };
    into(&file, "~/with space \u{416}.txt");
    into(&tree, "~/tree");
    assert_eq!(instance.ssh_ok(&["sh", "-c", "cat ~/tree/top.txt"]), "top");
    out_of("~/with space \u{416}.txt", &cli.scratch("file-back.txt"));
    out_of("~/tree", &cli.scratch("tree-back"));
    assert_eq!(std::fs::read(cli.scratch("file-back.txt")).unwrap(), b"a file");
    assert_eq!(read_tree(&cli.scratch("tree-back")), read_tree(&tree));

    // The disk is kept across a stop.
    instance.devvm_ok(&["stop", "cli-life"]);
    let listed = cli.devvm_ok(&["ls", "--json"]);
    let state = listed
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|info| info["name"] == "cli-life")
        .expect("cli-life is listed")["state"]
        .clone();
    assert_eq!(state, "stopped");
    instance.devvm_ok(&["start", "cli-life"]);
    assert_eq!(instance.ssh_ok(&["sh", "-c", "cat ~/tree/top.txt"]), "top");

    instance.devvm_ok(&["rm", "cli-life"]);
    assert!(!cli.devvm_ok(&["ls", "--all", "--json"]).contains("cli-life"));
    let gone = cli.devvm(&["ssh", "cli-life", "--", "true"]);
    assert_eq!(gone.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&gone.stderr).contains("devvm-error: not-found: "));
}

// Doctor and the fixture environment ==================================================================================

/// The lines of `devvm doctor` in the fixture environment.
fn doctor_lines(env: &CliEnv) -> (Output, Vec<String>) {
    let output = env.cli.devvm(&["doctor"]);
    let lines = stdout_lines(&output);
    (output, lines)
}

/// The line of check `name`: after `pass ` or `fail `.
fn line_of<'a>(lines: &'a [String], name: &str) -> &'a str {
    lines
        .iter()
        .find(|line| line.get(5..).is_some_and(|rest| rest.starts_with(&format!("{name}:"))))
        .unwrap_or_else(|| panic!("no line for {name}: {lines:?}"))
}

/// On the host the VM tests run on, every check passes, and the QEMU line gives the version and the hash of the build
/// devvm downloads.
#[skuld::test]
fn doctor_reports_accelerator_qemu_version_hash_and_free_space_each_with_pass_or_fail(
    #[fixture(cli_env)] env: &CliEnv,
) {
    let (output, lines) = doctor_lines(env);

    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("pass ") || line.starts_with("fail ")),
        "{lines:?}"
    );
    for name in [
        "accelerator",
        "free space (data)",
        "free space (cache)",
        "free space (state)",
        "qemu",
    ] {
        let line = line_of(&lines, name);
        assert!(line.starts_with("pass "), "{line}");
    }
    let qemu = line_of(&lines, "qemu");
    assert!(qemu.starts_with("pass qemu: QEMU "), "{qemu}");
    let hash = qemu.split("sha256 ").nth(1).expect("a hash").split(',').next().unwrap();
    assert!(
        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{qemu}"
    );
    assert_eq!(output.status.code(), Some(0), "{lines:?}");
}

#[skuld::test]
fn vm_fixture_roots_have_a_space_and_non_ascii_and_all_four_xdg_variables(#[fixture(cli_env)] env: &CliEnv) {
    let user = roots::base_dir().unwrap().join(UNICODE_DIR);
    let (_, lines) = doctor_lines(env);

    let roots = [
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
    ];
    for (var, what) in roots {
        let dir = env.cli.root_of(var);
        assert!(
            dir.starts_with(&user),
            "{var} {} is not below {}",
            dir.display(),
            user.display()
        );
        let text = dir.to_string_lossy();
        assert!(text.contains(' ') && !text.is_ascii(), "{var} {text}");
        // `devvm` is the directory devvm itself uses in each root.
        let line = line_of(&lines, &format!("free space ({what})"));
        assert!(
            line.ends_with(&format!(" free in {}", dir.join("devvm").display())),
            "{line}"
        );
    }
    let config = env.cli.root_of("XDG_CONFIG_HOME");
    assert!(
        config.starts_with(&user),
        "XDG_CONFIG_HOME {} is not below {}",
        config.display(),
        user.display()
    );
}
