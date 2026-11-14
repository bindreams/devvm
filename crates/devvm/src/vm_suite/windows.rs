//! A real Windows guest: the base built from the installer ISO that `DEVVM_WINDOWS_ISO` names, and an instance of it.
//!
//! Every test here joins the `windows_guest` group (`SKULD_LABELS=windows_guest` selects them), which only the
//! `vm-windows` job turns on. The base is built once per test process, and one instance of it is started for the tests
//! that only look at what a command does in the guest. Both run alone among the tests that boot a guest (`GUEST`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use super::fixture::{Launches, Roots, engine_reporting_to, run_ok, run_ok_as};
use crate::backend::qemu::sidecar::QemuInstall;
use crate::engine::Engine;
use crate::guest::GuestOs;
use crate::guest::windows::WindowsGuest;
use crate::guest::windows::elevate::TASK;
use crate::guest::windows::wrap::BROKER_EXE;
use crate::recipe::Recipe;
use crate::test_groups::{Group, WINDOWS_ISO_VAR, windows_guest};
use crate::test_harness::GUEST;
use crate::test_support::fixtures::MemoryProgress;
use crate::test_support::qemu::{qemu_sidecar, socket_dir};
use crate::{BuildOptions, Command, ExitStatus, Guest, Image, RunAs, StartOptions, Vm};

/// The Windows base of this test process, with the engine that built it and what it reported.
pub(super) struct WindowsBase {
    pub engine: Arc<Engine>,
    pub image: Image,
    pub progress: Arc<MemoryProgress>,
    recipe: Recipe,
    _roots: Roots,
}

/// The installer ISO the group's tests build from.
#[skuld::fixture(scope = process)]
pub(super) fn windows_iso(#[fixture(windows_guest)] _group: &Group) -> Result<PathBuf, String> {
    std::env::var_os(WINDOWS_ISO_VAR)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{WINDOWS_ISO_VAR} is not set"))
}

/// A recipe file `dir/r.devvm.yaml` that installs Windows from `iso` and runs `steps`, each one step's YAML.
pub(super) fn windows_recipe(dir: &Path, iso: &Path, steps: &[&str]) -> Recipe {
    let iso = serde_json::to_string(iso.to_str().expect("a UTF-8 ISO path")).unwrap();
    let steps = if steps.is_empty() {
        "steps: []\n".to_owned()
    } else {
        format!(
            "steps:\n{}",
            steps.iter().map(|step| format!("  - {step}\n")).collect::<String>()
        )
    };
    Recipe::parse(&format!("from: {{ iso: {iso} }}\n{steps}"), &dir.join("r.devvm.yaml")).unwrap()
}

/// What a test that boots the Windows guest asks for: Windows 11 wants 4 GiB.
pub(super) fn windows_options() -> StartOptions {
    StartOptions {
        cpus: Some(2),
        memory: Some(4 << 30),
        ..StartOptions::default()
    }
}

#[skuld::fixture(scope = process, serial = GUEST, labels = [crate::test_harness::GUEST])]
pub(super) fn windows_base(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(windows_iso)] iso: &PathBuf,
) -> Result<WindowsBase, String> {
    // A failed build is not tried again by every later test: skuld runs a fixture's setup again after it fails, and
    // each attempt is a full Windows install.
    static FAILED: OnceLock<String> = OnceLock::new();
    if let Some(error) = FAILED.get() {
        return Err(format!("the first build of this process failed: {error}"));
    }
    build_windows_base(install, iso).inspect_err(|error| {
        let _ = FAILED.set(error.clone());
    })
}

fn build_windows_base(install: &Arc<QemuInstall>, iso: &Path) -> Result<WindowsBase, String> {
    let roots = Roots::new(Some(&socket_dir()))?;
    let progress = Arc::new(MemoryProgress::echoing());
    let engine = engine_reporting_to(&roots.dirs, install, Arc::new(Launches::default()), progress.clone());
    let recipe = windows_recipe(&roots.dirs.state, iso, &[]);
    let image = engine
        .build(&recipe, &BuildOptions::default())
        .map_err(|e| format!("cannot build the Windows base from {}: {e}", iso.display()))?;
    Ok(WindowsBase {
        engine,
        image,
        progress,
        recipe,
        _roots: roots,
    })
}

/// One instance of the base, started once per test process; it runs until the process ends.
pub(super) struct SharedWindowsGuest {
    pub vm: Vm,
}

#[skuld::fixture(scope = process, serial = GUEST, labels = [crate::test_harness::GUEST])]
pub(super) fn shared_windows_guest(#[fixture(windows_base)] base: &WindowsBase) -> Result<SharedWindowsGuest, String> {
    let vm = base
        .image
        .start(&windows_options())
        .map_err(|e| format!("cannot start the Windows guest: {e}"))?;
    Ok(SharedWindowsGuest { vm })
}

/// What `script` prints in the guest, as the user, trimmed.
fn printed(guest: &SharedWindowsGuest, script: &str) -> String {
    let mut argv = vec!["pwsh", "-NoProfile", "-NonInteractive", "-Command"];
    argv.push(script);
    run_ok(&guest.vm, &argv).trim().to_owned()
}

const DAY: Duration = Duration::from_secs(24 * 3600);

// The base ============================================================================================================

#[skuld::test]
fn the_base_builds_from_the_evaluation_iso_and_reports_an_expiry(#[fixture(windows_base)] base: &WindowsBase) {
    let expires = base
        .image
        .chain
        .tip()
        .meta()
        .expires
        .expect("an evaluation base has an expiry");
    let now = SystemTime::now();
    // A fresh evaluation install has about 90 days of grace left.
    assert!(expires > now + 30 * DAY, "{expires:?}");
    assert!(expires < now + 120 * DAY, "{expires:?}");
}

#[skuld::test]
fn the_base_is_reused_by_a_second_build(#[fixture(windows_base)] base: &WindowsBase) {
    let generations = |image: &Image| -> Vec<_> {
        image
            .chain
            .refs()
            .iter()
            .map(|layer| (layer.key, layer.generation))
            .collect()
    };

    let again = base.engine.build(&base.recipe, &BuildOptions::default()).unwrap();

    assert_eq!(
        generations(&again),
        generations(&base.image),
        "the second build made a new layer"
    );
}

#[skuld::test]
fn the_specialize_pass_writes_a_progress_marker_to_the_serial_port(#[fixture(windows_base)] base: &WindowsBase) {
    let messages = base.progress.messages();
    assert!(
        messages.iter().any(|message| message == "Windows base: specialize"),
        "the specialize pass wrote no marker; stop: COM1 is not writable there: {messages:?}"
    );
}

#[skuld::test]
fn the_answer_disk_logs_fit_the_spare_room(#[fixture(windows_base)] base: &WindowsBase) {
    let messages = base.progress.messages();
    let line = messages
        .iter()
        .find(|message| message.starts_with("Windows base: WinPE logs use "))
        .unwrap_or_else(|| panic!("no report of the WinPE logs' sizes: {messages:?}"));
    eprintln!("{line}");
    let numbers: Vec<u64> = line.split_whitespace().filter_map(|word| word.parse().ok()).collect();
    let [used, room] = numbers[..] else {
        panic!("{line}");
    };
    assert!(used < room, "{line}");
}

// The guest ===========================================================================================================

#[skuld::test]
fn pwsh_resolves_by_name_in_the_built_base(#[fixture(shared_windows_guest)] guest: &SharedWindowsGuest) {
    assert_eq!(printed(guest, "$PSVersionTable.PSVersion.Major"), "7");
}

#[skuld::test]
fn a_flush_through_the_broker_succeeds_with_data_to_flush(#[fixture(shared_windows_guest)] guest: &SharedWindowsGuest) {
    printed(
        guest,
        "[IO.File]::WriteAllBytes('C:/ProgramData/devvm/flush-test.bin', (New-Object byte[] 67108864))",
    );

    let output = guest.vm.run(&WindowsGuest.flush_command()).unwrap();

    assert_eq!(output.status, ExitStatus::Code(0), "{}", output.stderr_lossy());
}

#[skuld::test]
fn a_medium_process_reads_the_helper_tasks_state_starts_it_with_an_argument_and_a_started_instance_is_running_at_once(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    // The user's own process reads the task: `3` is ready, with no instance running.
    let state = printed(
        guest,
        &format!(
            "$service = New-Object -ComObject 'Schedule.Service'; $service.Connect(); \
             [int]$service.GetFolder('\\').GetTask('{TASK}').State"
        ),
    );
    assert_eq!(state, "3", "the helper task is not ready");

    // The elevated path starts the task with the request's id, and a runner that is not counted as running at once
    // would make some of these fail with "the elevated helper did not run the command".
    let admin = "[Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)";
    assert_eq!(
        printed(guest, admin),
        "False",
        "an administrator's Medium token does not have the group"
    );
    for round in 1..=5 {
        let output = run_ok_as(
            &guest.vm,
            RunAs::Elevated,
            &["pwsh", "-NoProfile", "-NonInteractive", "-Command", admin],
        );
        assert_eq!(
            output.trim(),
            "True",
            "round {round}: the helper runs the command at High"
        );
    }
}

#[skuld::test]
fn the_boot_task_waits_for_the_agent_socket_and_the_agent_survives_a_silent_connect(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    // sshd answers only after the boot task saw the agent's socket accept, so reaching the guest at all, in the user's
    // session, is the boot task's wait having ended. A connect that sends nothing must not end the agent.
    let silent = "$socket = [Net.Sockets.Socket]::new('Unix', 'Stream', 'Unspecified'); \
                  $socket.Connect([Net.Sockets.UnixDomainSocketEndPoint]::new((Join-Path $env:LOCALAPPDATA 'ssh-broker\\agent.sock'))); \
                  $socket.Close(); 'connected'";
    assert_eq!(printed(guest, silent), "connected");

    assert_eq!(
        printed(guest, "[Diagnostics.Process]::GetCurrentProcess().SessionId"),
        "1"
    );
    assert_eq!(printed(guest, silent), "connected", "the agent still accepts");
}

#[skuld::test]
fn ssh_broker_verify_passes_on_the_clean_guest(#[fixture(shared_windows_guest)] guest: &SharedWindowsGuest) {
    let output = guest.vm.run(&Command::new([BROKER_EXE, "verify"])).unwrap();

    assert_eq!(
        output.status,
        ExitStatus::Code(0),
        "{}{}",
        output.stdout_lossy(),
        output.stderr_lossy()
    );
}
