//! Commands that need no VM and no QEMU, through the real binary.

use std::path::Path;
use std::process::{Command, Output, Stdio};

#[path = "support/home.rs"]
mod home;
use home::{Home, home, id, one_error_line, stdout_lines};
#[path = "support/isolate.rs"]
mod isolate;
#[path = "support/unicode.rs"]
mod unicode;

const MANIFEST: &str = include_str!("../../devvm/qemu-manifest.toml");

/// Read by the stub `ssh`: the code it exits with, and where it records its arguments.
const STUB_EXIT: &str = "DEVVM_STUB_SSH_EXIT";
const STUB_RECORD: &str = "DEVVM_STUB_SSH_RECORD";

fn main() {
    if let Some(code) = act_as_ssh_when_installed_as_one() {
        std::process::exit(code);
    }
    skuld::run_all();
}

/// A copy of this executable installed as `ssh` is the stub: it prints its arguments as a JSON array on its stdout,
/// records them in the file `STUB_RECORD` names, and exits with the code `STUB_EXIT` names. The environment of the
/// `ssh` a devvm runs is devvm's own, so the file name is how it knows to be one.
fn act_as_ssh_when_installed_as_one() -> Option<i32> {
    let exe = std::env::current_exe().ok()?;
    if exe.file_stem()? != "ssh" {
        return None;
    }
    let args = serde_json::to_string(&std::env::args().skip(1).collect::<Vec<_>>()).unwrap();
    println!("{args}");
    if let Some(record) = std::env::var_os(STUB_RECORD) {
        std::fs::write(record, &args).unwrap();
    }
    Some(
        std::env::var(STUB_EXIT)
            .ok()
            .and_then(|code| code.parse().ok())
            .unwrap_or(0),
    )
}

#[skuld::test]
fn doctor_prints_every_check_then_exits_125_iff_any_line_says_fail() {
    let home = home();
    let cwd = home.project("a");

    let output = home.devvm(&cwd, &["doctor"]);

    let lines = stdout_lines(&output);
    let failed = lines.iter().filter(|line| line.starts_with("fail ")).count();
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("pass ") || line.starts_with("fail ")),
        "{lines:?}"
    );
    let names = [
        "accelerator",
        "free space (data)",
        "free space (cache)",
        "free space (state)",
        "ssh",
        "git",
        "qemu",
    ];
    for name in names {
        assert!(
            lines.iter().any(|line| line[5..].starts_with(&format!("{name}:"))),
            "no line for {name}: {lines:?}"
        );
    }
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    if failed == 0 {
        assert_eq!(output.status.code(), Some(0), "{stderr:?}");
    } else {
        assert_eq!(output.status.code(), Some(125), "{stderr:?}");
        assert_eq!(
            stderr.lines().last(),
            Some(format!("devvm-error: unavailable: {failed} check(s) failed").as_str()),
            "{stderr:?}"
        );
    }
    // Nothing is downloaded in a fresh root, so a build in the manifest is a pass.
    let qemu = lines.iter().find(|line| line[5..].starts_with("qemu:")).unwrap();
    if MANIFEST.contains(&format!("[targets.{}]", devvm::TARGET)) {
        assert!(qemu.starts_with("pass "), "{qemu}");
    }
}

#[skuld::test]
fn doctor_with_an_unreadable_config_prints_every_line_and_the_config_text_on_the_qemu_line() {
    let home = home();
    let cwd = home.project("a");
    let config = home.config_file();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "qemu: [not, a, mapping]").unwrap();

    let output = home.devvm(&cwd, &["doctor"]);

    let lines = stdout_lines(&output);
    for name in [
        "accelerator",
        "free space (data)",
        "free space (cache)",
        "free space (state)",
        "ssh",
        "git",
        "qemu",
    ] {
        assert!(
            lines.iter().any(|line| line[5..].starts_with(&format!("{name}:"))),
            "no line for {name}: {lines:?}"
        );
    }
    let qemu = lines.iter().find(|line| line[5..].starts_with("qemu:")).unwrap();
    assert!(qemu.starts_with("fail "), "{qemu}");
    assert!(qemu.contains("config.yaml"), "{qemu}");
    assert_eq!(output.status.code(), Some(125), "{:?}", output.stderr);
}

// Interactive ssh =====================================================================================================

/// A directory holding a copy of this executable as `ssh`.
fn stub_ssh_dir(home: &Home) -> std::path::PathBuf {
    let dir = home.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let stub = dir.join(format!("ssh{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(std::env::current_exe().unwrap(), stub).unwrap();
    dir
}

/// `devvm <args>` with the stub `ssh` first on `PATH`; it exits with `code` and records its arguments in `record`.
fn devvm_with_stub_ssh(home: &Home, cwd: &Path, args: &[&str], code: i32, record: &Path) -> Output {
    let mut path = vec![stub_ssh_dir(home)];
    path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_devvm"));
    command
        .current_dir(cwd)
        .args(args)
        .env_remove("DEVVM_STATUS_FILE")
        .env("PATH", std::env::join_paths(path).unwrap())
        .env(STUB_EXIT, code.to_string())
        .env(STUB_RECORD, record);
    isolate::isolate(&mut command, &home.root);
    command.stdin(Stdio::null()).output().unwrap()
}

/// Gives the seeded instance `name` a machine that the backend finds: the process of this test, which is alive for as
/// long as the test runs. It has an SSH endpoint, the four key files and the `ssh` configuration files, which is what
/// `Instance::ssh_config` reads.
fn seed_a_running_machine(home: &Home, name: &str) {
    let dir = home.state().join("instances").join(name);
    let process = cosca::identity::ProcessId::current().to_record().unwrap();
    let machine = serde_json::json!({
        "process": process,
        "qmp": "",
        "ssh": [{ "endpoint": "normal", "addr": "127.0.0.1:2222" }],
        "serial_log": dir.join("serial.log"),
        "leftovers": [],
        "claim": { "path": dir.join("claim"), "id": "0:0" },
    });
    let path = dir.join("record.json");
    let mut record: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    record["machine"] = machine;
    std::fs::write(path, record.to_string()).unwrap();
    let keys = dir.join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    for file in ["client", "client.pub", "host", "host.pub"] {
        std::fs::write(keys.join(file), "key").unwrap();
    }
    let ssh = home.state().join("ssh");
    std::fs::create_dir_all(&ssh).unwrap();
    for file in ["config".to_owned(), format!("{name}.conf")] {
        std::fs::write(ssh.join(file), "").unwrap();
    }
}

#[skuld::test]
fn interactive_ssh_of_a_stopped_instance_fails_unavailable_naming_it_and_starts_no_ssh() {
    let home = home();
    let cwd = home.project("a");
    home.seed("box", &id(1), &cwd, "none");
    let record = home.root.join("ssh-ran.json");

    let output = devvm_with_stub_ssh(&home, &cwd, &["ssh", "box"], 0, &record);

    assert_eq!(output.status.code(), Some(125));
    let line = one_error_line(&output, "unavailable");
    assert!(line.contains("box") && line.contains("start it"), "{line}");
    let started = std::fs::symlink_metadata(&record);
    assert_eq!(
        started.as_ref().err().map(std::io::Error::kind),
        Some(std::io::ErrorKind::NotFound),
        "the system ssh was started: {started:?}"
    );
}

#[skuld::test]
fn interactive_ssh_runs_system_ssh_with_the_generated_config_and_passes_on_its_exit_code_and_stdout() {
    let home = home();
    let cwd = home.project("a");
    home.seed("box", &id(1), &cwd, "none");
    seed_a_running_machine(&home, "box");
    let record = home.root.join("ssh-ran.json");

    let output = devvm_with_stub_ssh(&home, &cwd, &["ssh", "box"], 7, &record);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(7), "{stderr}");
    let config = home.state().join("ssh").join("config");
    let expected = serde_json::json!(["-F", config, "box"]);
    // The child's stdout is this devvm's stdout, so the console is inherited and nothing is captured in between.
    let printed: serde_json::Value = serde_json::from_str(&stdout_lines(&output)[0]).unwrap();
    assert_eq!(printed, expected);
    let recorded: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&record).unwrap()).unwrap();
    assert_eq!(recorded, expected);
}

// Commands that fail before any VM ====================================================================================

#[skuld::test]
fn cp_of_two_local_paths_is_a_usage_error() {
    let home = home();
    let cwd = home.project("a");

    let output = home.devvm(&cwd, &["cp", "a.txt", "b.txt"]);

    assert_eq!(output.status.code(), Some(125));
    one_error_line(&output, "usage");
}

#[skuld::test]
fn commands_that_name_no_instance_exit_125_not_found() {
    let home = home();
    let cwd = home.project("a");
    let commands: [&[&str]; 5] = [
        &["ssh", "ghost"],
        &["ssh", "ghost", "--", "true"],
        &["cp", "a.txt", "ghost:~/a.txt"],
        &["start", "ghost"],
        &["stop", "ghost"],
    ];
    for args in commands {
        let output = home.devvm(&cwd, args);
        assert_eq!(output.status.code(), Some(125), "{args:?}");
        let line = one_error_line(&output, "not-found");
        assert!(line.contains("ghost"), "{line}");
    }
}

#[skuld::test]
fn stop_of_a_stopped_instance_does_nothing() {
    let home = home();
    let cwd = home.project("a");
    home.seed("box", &id(1), &cwd, "none");

    let output = home.devvm(&cwd, &["stop", "box"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}
