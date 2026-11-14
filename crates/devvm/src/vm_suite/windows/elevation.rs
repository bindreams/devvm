//! Elevated commands: the helper task and its runner (`eb-runner.ps1`) answer each request on their own, stream, end with
//! their caller, and end with an error when no runner will ever serve a request. A test that kills the runner or disables
//! the task boots an instance of its own.

use std::io::{self, Write};
use std::sync::mpsc::{self, RecvTimeoutError};

use super::bounds::STATE_BOUND;
use super::windows_options;
use super::{SharedWindowsGuest, WindowsBase, printed, pwsh, ran, shared_windows_guest, until, windows_base};
use crate::guest::windows::elevate::TASK;
use crate::vm_suite::fixture::Up;
use crate::vm_suite::waits::{answer_to, step};
use crate::{Command, ExitStatus, Guest, RunAs};

/// The directory the helper keeps its requests in; it is empty when no request is pending.
const REQUESTS: &str = r"C:\ProgramData\devvm\eb";

/// How many requests the helper holds.
fn pending(guest: &dyn Guest) -> String {
    printed(
        guest,
        &format!(r"(Get-ChildItem -LiteralPath '{REQUESTS}' -Force | Measure-Object).Count"),
    )
}

/// The flag file `name` in the guest.
fn flag(name: &str) -> String {
    format!("C:/ProgramData/devvm/{name}.flag")
}

/// PowerShell that waits in the guest until the flag `name` exists. It has no bound: the host makes the flag, on every
/// path of its test, so nothing waits for ever and no wait of the guest's decides what the host asserts.
fn wait_for_flag(name: &str) -> String {
    format!(
        "while (-not (Test-Path -LiteralPath {})) {{ Start-Sleep -Milliseconds 100 }}",
        flag(name)
    )
}

/// Makes the flag file `name` from outside the command that waits for it, so that command goes on. The result is only
/// printed; a guest that cannot make the file has failed the command it serves too.
fn release(guest: &dyn Guest, name: &str) {
    let script = format!("Set-Content -LiteralPath {} -Value x", flag(name));
    if let Err(error) = ran(guest, &format!("making {name}.flag"), &pwsh(RunAs::User, &script)) {
        eprintln!("cannot release {name}.flag: {error}");
    }
}

/// Makes the flag `name` when dropped, so the command that waits for it ends with the test, whether the test passed or
/// failed.
struct Releases<'a> {
    guest: &'a dyn Guest,
    name: &'static str,
}

impl Drop for Releases<'_> {
    fn drop(&mut self) {
        release(self.guest, self.name);
    }
}

/// The flag the two commands of the test below wait for.
const BOTH_RUNNING: &str = "both-running";

/// An elevated command that makes its own flag file, then waits for [`BOTH_RUNNING`].
fn rendezvous(own: &str, code: i32) -> Command {
    pwsh(
        RunAs::Elevated,
        &format!(
            "Set-Content -LiteralPath {} -Value x; {}; \
             Write-Output 'out {own}'; [Console]::Error.WriteLine('err {own}'); exit {code}",
            flag(own),
            wait_for_flag(BOTH_RUNNING)
        ),
    )
}

#[skuld::test]
fn windows_two_elevated_commands_at_once_each_get_their_own_output_and_status(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    printed(
        &guest.vm,
        &format!(
            "Remove-Item -LiteralPath {}, {}, {} -ErrorAction Ignore",
            flag("a"),
            flag("b"),
            flag(BOTH_RUNNING)
        ),
    );
    let (a, b) = (rendezvous("a", 3), rendezvous("b", 5));

    step("running the two commands");
    // Each command makes its flag and waits. The host sees both flags, which only two commands running at once make, and
    // then lets them end. A product that runs them one after the other fails the wait for the second flag. The guard makes
    // the last flag on every path, so the commands end either way.
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| ran(&guest.vm, "the elevated command a", &a));
        let b = scope.spawn(|| ran(&guest.vm, "the elevated command b", &b));
        let go = Releases {
            guest: &guest.vm,
            name: BOTH_RUNNING,
        };
        until("both elevated commands running at once", || {
            printed(
                &guest.vm,
                &format!(
                    "(Test-Path -LiteralPath {}) -and (Test-Path -LiteralPath {})",
                    flag("a"),
                    flag("b")
                ),
            ) == "True"
        });
        drop(go);
        (a.join().unwrap(), b.join().unwrap())
    });
    let (a, b) = (a.unwrap(), b.unwrap());

    for (name, seen, code) in [("a", a, 3), ("b", b, 5)] {
        assert_eq!(seen.status, ExitStatus::Code(code), "{name}: {}", seen.stderr_lossy());
        assert_eq!(seen.stdout_lossy().trim(), format!("out {name}"));
        assert!(
            seen.stderr_lossy().contains(&format!("err {name}")),
            "{name}: {}",
            seen.stderr_lossy()
        );
    }
    printed(
        &guest.vm,
        &format!(
            "Remove-Item -LiteralPath {}, {}, {}",
            flag("a"),
            flag("b"),
            flag(BOTH_RUNNING)
        ),
    );
}

/// Tells the test which of the command's output it has seen.
struct Watch {
    seen: Vec<u8>,
    first: Option<mpsc::Sender<()>>,
}

impl Write for Watch {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.seen.extend_from_slice(bytes);
        if self.seen.windows(5).any(|window| window == b"first")
            && let Some(first) = self.first.take()
        {
            let _ = first.send(());
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[skuld::test]
fn windows_an_elevated_command_streams_its_output_as_it_runs(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    printed(
        &guest.vm,
        &format!("Remove-Item -LiteralPath {} -ErrorAction Ignore", flag("go")),
    );
    // The command ends only once the host has the first line: the host makes the flag then, and on every other path too.
    // A product that does not stream never shows the host the first line: the wait for it ends at its bound, the flag is
    // made, and the test fails.
    let wait = wait_for_flag("go");
    let command = pwsh(
        RunAs::Elevated,
        &format!(
            "[Console]::Out.WriteLine('first'); [Console]::Out.Flush(); {wait}; [Console]::Out.WriteLine('second')"
        ),
    );
    let (sender, first) = mpsc::channel();

    let _guard = answer_to("the elevated command that streams");

    let ((status, seen), heard) = std::thread::scope(|scope| {
        let run = scope.spawn(|| {
            let mut watch = Watch {
                seen: Vec::new(),
                first: Some(sender),
            };
            let status = guest.vm.run_to(&command, &mut watch, &mut io::sink());
            (status, watch.seen)
        });
        // The first line comes while the command still runs. When the run fails first, the sender is dropped and the
        // flag is made all the same, so a command that did start does not wait for it for ever.
        let go = Releases {
            guest: &guest.vm,
            name: "go",
        };
        let heard = first.recv_timeout(STATE_BOUND);
        drop(go);
        (run.join().unwrap(), heard)
    });
    assert_ne!(
        heard,
        Err(RecvTimeoutError::Timeout),
        "the first line did not reach the host within {STATE_BOUND:?}"
    );
    let status = status.unwrap();

    let text = String::from_utf8_lossy(&seen);
    assert_eq!(status, ExitStatus::Code(0), "{text}");
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    assert_eq!(
        lines,
        ["first", "second"],
        "the output reached the host while the command ran"
    );
    printed(&guest.vm, &format!("Remove-Item -LiteralPath {}", flag("go")));
}

/// Fails its write once the first line is whole, as a reader that went away does.
struct Leaver {
    seen: Vec<u8>,
}

impl Write for Leaver {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.seen.extend_from_slice(bytes);
        if self.seen.contains(&b'\n') {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "the reader went away"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The elevated child the host started, ended by its process id when the test fails before the host has seen it end:
/// nothing is left running on the shared guest.
struct Child<'a> {
    guest: &'a dyn Guest,
    pid: &'a str,
    ended: bool,
}

impl Drop for Child<'_> {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        let script = format!("Stop-Process -Id {} -Force -ErrorAction Ignore", self.pid);
        if let Err(error) = ran(self.guest, "stopping the child", &pwsh(RunAs::Elevated, &script)) {
            eprintln!("cannot stop process {}: {error}", self.pid);
        }
    }
}

#[skuld::test]
fn windows_an_elevated_child_ends_when_the_client_goes_away(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    // The command prints its process id, then runs until it is ended. It has no bound of its own: a bound that ended it
    // would be the one thing that makes the process go, and the test asserts that devvm does. When devvm does not end
    // it, the wait below for the process to end fails the test, and the test ends the child by its id.
    let command = pwsh(
        RunAs::Elevated,
        "[Console]::Out.WriteLine($PID); [Console]::Out.Flush(); while ($true) { Start-Sleep -Milliseconds 100 }",
    );
    let mut leaver = Leaver { seen: Vec::new() };

    let guard = answer_to("the elevated child that prints its process id and loops");
    // devvm ends the command when its output has nowhere to go; the status that never comes is the guest's.
    let result = guest.vm.run_to(&command, &mut leaver, &mut io::sink());
    drop(guard);

    let pid = String::from_utf8_lossy(&leaver.seen).trim().to_owned();
    assert!(
        pid.chars().all(|c| c.is_ascii_digit()) && !pid.is_empty(),
        "{pid:?} {result:?}"
    );
    let mut child = Child {
        guest: &guest.vm,
        pid: &pid,
        ended: false,
    };
    step("waiting for the process to end");
    until("the command's process ending", || {
        printed(
            &guest.vm,
            &format!("if (Get-Process -Id {pid} -ErrorAction Ignore) {{ 'running' }} else {{ 'gone' }}"),
        ) == "gone"
    });
    child.ended = true;
    // The runner removes the request of a caller that ended.
    until("the runner removing the request", || pending(&guest.vm) == "0");
}

// A request that gets no answer =======================================================================================

#[skuld::test]
fn windows_an_elevated_command_whose_runner_dies_ends_with_an_error_not_a_wait(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    step("booting an instance");
    let up = Up::new(&base.image, "w-runner-dies", &windows_options()).unwrap();
    // The command ends the runner that serves it: the process that started `eb-runner.ps1`, never itself. It then loops
    // with no bound, so only devvm can end the caller's wait: a guest-side sleep that ran out would end the test's wait
    // for it.
    let command = pwsh(
        RunAs::Elevated,
        r#"Get-CimInstance Win32_Process -Filter "Name = 'pwsh.exe'" | Where-Object { $_.ProcessId -ne $PID -and $_.CommandLine -match 'eb-runner\.ps1' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }; while ($true) { Start-Sleep -Milliseconds 100 }"#,
    );

    let seen = ran(&*up, "the command that ends its runner", &command).unwrap();

    // The claim file is released by the OS when its runner dies, and `done` never came: the caller says so.
    assert_eq!(seen.status, ExitStatus::Code(126), "{}", seen.stderr_lossy());
    assert!(
        seen.stderr_lossy()
            .contains("devvm: the elevated runner ended without a result"),
        "{}",
        seen.stderr_lossy()
    );
    assert_eq!(seen.stdout_lossy(), "");
    assert_eq!(pending(&*up), "0", "the caller removes a request whose runner died");
}

/// What the helper task is turned into before the request: disabled, or removed.
struct Change {
    what: &'static str,
    script: String,
    message: &'static str,
}

#[skuld::test]
fn windows_an_elevated_request_the_task_never_runs_ends_with_an_error_not_a_wait(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    let task = "$service = New-Object -ComObject 'Schedule.Service'; $service.Connect(); \
                $folder = $service.GetFolder('\\'); ";
    let changes = [
        Change {
            what: "disabled",
            script: format!("{task}$folder.GetTask('{TASK}').Enabled = $false"),
            message: "devvm: the elevated helper did not",
        },
        Change {
            what: "removed",
            script: format!("{task}$folder.DeleteTask('{TASK}', 0)"),
            message: "devvm: the elevated helper is not available",
        },
    ];
    for change in changes {
        step(&format!("booting an instance, then the helper {}", change.what));
        let up = Up::new(&base.image, &format!("w-task-{}", change.what), &windows_options()).unwrap();
        // The last request the task serves.
        let done = ran(
            &*up,
            &format!("the command that has the helper {}", change.what),
            &pwsh(RunAs::Elevated, &change.script),
        )
        .unwrap();
        assert_eq!(
            done.status,
            ExitStatus::Code(0),
            "{}: {}",
            change.what,
            done.stderr_lossy()
        );

        let seen = ran(
            &*up,
            &format!("a command with the helper {}", change.what),
            &pwsh(
                RunAs::Elevated,
                "Set-Content -LiteralPath C:/ProgramData/devvm/ran.txt -Value x",
            ),
        )
        .unwrap();

        // Neither a wait nor a run: an answer that names the helper, and the command did not run.
        assert_eq!(
            seen.status,
            ExitStatus::Code(126),
            "{}: {}",
            change.what,
            seen.stderr_lossy()
        );
        assert!(
            seen.stderr_lossy().contains(change.message),
            "{}: {}",
            change.what,
            seen.stderr_lossy()
        );
        assert_eq!(seen.stdout_lossy(), "");
        assert_eq!(
            printed(&*up, "Test-Path -LiteralPath C:/ProgramData/devvm/ran.txt"),
            "False",
            "the command ran with the helper {}",
            change.what
        );
        assert_eq!(pending(&*up), "0", "the caller removes the request nobody will serve");
    }
}
