//! A real Windows guest: the base built from the installer ISO that `DEVVM_WINDOWS_ISO` names, and an instance of it.
//!
//! Every test here joins the `windows_guest` group (`SKULD_LABELS=windows_guest` selects them), which only the
//! `vm-windows` job turns on. The base is built once per test process, and one instance of it is started for the tests
//! that only look at what a command does in the guest; a test that stops, kills, disables, resets or restarts something
//! boots an instance of its own. All of them run alone among the tests that boot a guest (`GUEST`).
//!
//! The submodules hold the suites by what they look at: the user's session (`session`), the exec table (`commands`),
//! elevation (`elevation`), the instance's identity (`seed`), restarts (`boots`) and the machine's own state, which is
//! the account, the disk, the expiry, the terminal and the windows (`machine`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use super::fixture::{Launches, Roots, engine_reporting_to, run_ok_as};
use super::waits::{answer_to, step};
use crate::backend::qemu::sidecar::QemuInstall;
use crate::engine::Engine;
use crate::guest::GuestOs;
use crate::guest::windows::WindowsGuest;
use crate::guest::windows::elevate::TASK;
use crate::guest::windows::wrap::BROKER_EXE;
use crate::recipe::{Recipe, Shell};
use crate::test_groups::{Group, WINDOWS_ISO_VAR, windows_guest};
use crate::test_harness::GUEST;
use crate::test_support::fixtures::MemoryProgress;
use crate::test_support::qemu::{qemu_sidecar, socket_dir};
use crate::{BuildOptions, Command, Error, ExitStatus, Guest, Image, Output, RunAs, StartOptions, Vm};

mod boots;
mod bounds;
mod commands;
mod elevation;
mod machine;
mod seed;
mod session;
mod shared;

use bounds::{GUEST_BOUND, STATE_BOUND};
use shared::{first_try, recipe_text};

/// The Windows base of this test process, with the engine that built it and what it reported.
pub(super) struct WindowsBase {
    pub engine: Arc<Engine>,
    pub image: Image,
    pub progress: Arc<MemoryProgress>,
    /// When the build reported the license read its base's expiry comes from: taken as the build returns, so a later
    /// build through `engine` cannot move it.
    pub license_read_at: SystemTime,
    /// The machines this base's engine launched.
    pub launches: Arc<Launches>,
    pub recipe: Recipe,
    pub roots: Roots,
}

/// The installer ISO the group's tests build from.
#[skuld::fixture(scope = process, deref)]
pub(super) fn windows_iso(#[fixture(windows_guest)] _group: &Group) -> Result<PathBuf, String> {
    std::env::var_os(WINDOWS_ISO_VAR)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{WINDOWS_ISO_VAR} is not set"))
}

/// A recipe file `dir/r.devvm.yaml` that installs Windows from `iso` and runs `steps`, each one step's YAML.
pub(super) fn windows_recipe(dir: &Path, iso: &Path, steps: &[&str]) -> Recipe {
    let text = recipe_text(iso, steps).expect("a recipe for a UTF-8 ISO path");
    Recipe::parse(&text, &dir.join("r.devvm.yaml")).unwrap()
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
    #[fixture(windows_iso)] iso: &Path,
) -> Result<WindowsBase, String> {
    static FAILED: OnceLock<String> = OnceLock::new();
    first_try(&FAILED, || build_windows_base(install, iso))
}

fn build_windows_base(install: &Arc<QemuInstall>, iso: &Path) -> Result<WindowsBase, String> {
    let roots = Roots::new(Some(&socket_dir()))?;
    let progress = Arc::new(MemoryProgress::echoing());
    let launches = Arc::new(Launches::default());
    let engine = engine_reporting_to(&roots.dirs, install, launches.clone(), progress.clone());
    let recipe = windows_recipe(&roots.dirs.state, iso, &[]);
    let image = engine
        .build(&recipe, &BuildOptions::default())
        .map_err(|e| format!("cannot build the Windows base from {}: {e}", iso.display()))?;
    let (license_read_at, _) = progress
        .messages_at()
        .into_iter()
        .rfind(|(_, line)| line.starts_with("Windows base: license status"))
        .ok_or("the build reported no license read")?;
    Ok(WindowsBase {
        engine,
        image,
        progress,
        license_read_at,
        launches,
        recipe,
        roots,
    })
}

/// One instance of the base, started once per test process; it runs until the process ends.
pub(super) struct SharedWindowsGuest {
    pub vm: Vm,
}

#[skuld::fixture(scope = process, serial = GUEST, labels = [crate::test_harness::GUEST])]
pub(super) fn shared_windows_guest(#[fixture(windows_base)] base: &WindowsBase) -> Result<SharedWindowsGuest, String> {
    static FAILED: OnceLock<String> = OnceLock::new();
    first_try(&FAILED, || {
        step("booting the shared instance");
        let vm = base
            .image
            .start(&windows_options())
            .map_err(|e| format!("cannot start the Windows guest: {e}"))?;
        Ok(SharedWindowsGuest { vm })
    })
}

/// `script` as the command a `shell: pwsh` step would run: UTF-8 output, and a script block of its own.
pub(super) fn pwsh(run_as: RunAs, script: &str) -> Command {
    Command {
        argv: WindowsGuest
            .shell_argv(Some(Shell::Pwsh), script)
            .expect("PowerShell is a shell of a Windows guest"),
        run_as,
        cwd: None,
    }
}

/// What `script` prints in the guest, as the user, trimmed.
pub(super) fn printed(guest: &dyn Guest, script: &str) -> String {
    printed_as(guest, RunAs::User, script)
}

/// [`printed`] with the privilege `run_as`.
pub(super) fn printed_as(guest: &dyn Guest, run_as: RunAs, script: &str) -> String {
    let command = pwsh(run_as, script);
    let argv: Vec<&str> = command.argv.iter().map(String::as_str).collect();
    let _watch = answer_to(&command.argv);
    run_ok_as(guest, run_as, &argv).trim().to_owned()
}

/// Runs `command` in `guest`, which must answer within [`ANSWER_BOUND`](super::waits::ANSWER_BOUND).
pub(super) fn ran(guest: &dyn Guest, command: &Command) -> Result<Output, Error> {
    let _watch = answer_to(&command.argv);
    guest.run(command)
}

/// The pause between two looks at a condition a wait re-checks. It paces the wait; it never ends it.
pub(super) const PACE: Duration = Duration::from_millis(200);

/// Waits until `condition` holds. A condition that does not hold within [`STATE_BOUND`] fails the test with `what` did
/// not happen; the bound only ends the wait.
pub(super) fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + STATE_BOUND;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "{what} did not happen within {STATE_BOUND:?}"
        );
        std::thread::sleep(PACE);
    }
}

/// PowerShell that waits in the guest until `condition` holds. After [`GUEST_BOUND`], which is longer than every host
/// bound that waits on the effect, it prints that `what` did not happen and ends the wait, not the command or the shell
/// that runs it: the host fails the test long before, and a bound never decides what the host asserts. `$false` is a
/// wait that only the bound ends.
pub(super) fn guest_until(what: &str, condition: &str) -> String {
    let seconds = GUEST_BOUND.as_secs();
    let what = what.replace('\'', "''");
    format!(
        "$deadline = (Get-Date).AddSeconds({seconds}); \
         while (-not ({condition})) {{ \
         if ((Get-Date) -gt $deadline) {{ [Console]::Error.WriteLine('devvm test: {what} did not happen'); break }}; \
         Start-Sleep -Milliseconds 100 }}"
    )
}

/// The integrity level SID of a Medium token and of a High one.
pub(super) const MEDIUM: &str = "S-1-16-8192";
pub(super) const HIGH: &str = "S-1-16-12288";

/// Prints `session=<id> integrity=<SID>` of the process that runs it. The SID is read from `whoami /groups`, which
/// prints it whatever the guest's language.
pub(super) const FACTS: &str = r#"$integrity = (whoami /groups | Select-String -Pattern 'S-1-16-\d+').Matches.Value; "session=$([Diagnostics.Process]::GetCurrentProcess().SessionId) integrity=$integrity""#;

/// The session and the integrity level a command of the guest runs in.
pub(super) fn facts(guest: &dyn Guest, run_as: RunAs) -> String {
    printed_as(guest, run_as, FACTS)
}

/// What [`facts`] says of a user's command: in session 1, at Medium integrity.
pub(super) fn user_facts() -> String {
    format!("session=1 integrity={MEDIUM}")
}

/// What [`facts`] says of an elevated command.
pub(super) fn elevated_facts() -> String {
    format!("session=1 integrity={HIGH}")
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
    assert_eq!(printed(&guest.vm, "$PSVersionTable.PSVersion.Major"), "7");
}

#[skuld::test]
fn a_flush_through_the_broker_succeeds_with_data_to_flush(#[fixture(shared_windows_guest)] guest: &SharedWindowsGuest) {
    printed(
        &guest.vm,
        "[IO.File]::WriteAllBytes('C:/ProgramData/devvm/flush-test.bin', (New-Object byte[] 67108864))",
    );

    let output = ran(&guest.vm, &WindowsGuest.flush_command()).unwrap();

    assert_eq!(output.status, ExitStatus::Code(0), "{}", output.stderr_lossy());
}

#[skuld::test]
fn a_medium_process_reads_the_helper_tasks_state_starts_it_with_an_argument_and_a_started_instance_is_running_at_once(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    // The user's own process reads the task: `3` is ready, with no instance running.
    let state = printed(
        &guest.vm,
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
        printed(&guest.vm, admin),
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
    assert_eq!(printed(&guest.vm, silent), "connected");

    assert_eq!(
        printed(&guest.vm, "[Diagnostics.Process]::GetCurrentProcess().SessionId"),
        "1"
    );
    assert_eq!(printed(&guest.vm, silent), "connected", "the agent still accepts");
}

#[skuld::test]
fn ssh_broker_verify_passes_on_the_clean_guest(#[fixture(shared_windows_guest)] guest: &SharedWindowsGuest) {
    let output = ran(&guest.vm, &Command::new([BROKER_EXE, "verify"])).unwrap();

    assert_eq!(
        output.status,
        ExitStatus::Code(0),
        "{}{}",
        output.stdout_lossy(),
        output.stderr_lossy()
    );
}
