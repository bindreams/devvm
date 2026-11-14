//! Elevated commands: the helper task and its runner (`eb-runner.ps1`) answer each request on their own, stream, end with
//! their caller, and end with an error when no runner will ever serve a request. A test that kills the runner or disables
//! the task boots an instance of its own.

use std::io::{self, Write};
use std::sync::mpsc;

use super::windows_options;
use super::{SharedWindowsGuest, WindowsBase, printed, pwsh, shared_windows_guest, until, windows_base};
use crate::guest::windows::elevate::TASK;
use crate::vm_suite::fixture::Up;
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

/// An elevated command that makes its own flag file and waits for the other's: both are running at the same time, or
/// neither ends.
fn rendezvous(own: &str, other: &str, code: i32) -> Command {
    pwsh(
        RunAs::Elevated,
        &format!(
            "Set-Content -LiteralPath C:/ProgramData/devvm/{own}.flag -Value x; \
             while (-not (Test-Path -LiteralPath C:/ProgramData/devvm/{other}.flag)) {{ Start-Sleep -Milliseconds 100 }}; \
             Write-Output 'out {own}'; [Console]::Error.WriteLine('err {own}'); exit {code}"
        ),
    )
}

#[skuld::test]
fn windows_two_elevated_commands_at_once_each_get_their_own_output_and_status(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    printed(
        &guest.vm,
        "Remove-Item -LiteralPath C:/ProgramData/devvm/a.flag, C:/ProgramData/devvm/b.flag -ErrorAction Ignore",
    );
    let (a, b) = (rendezvous("a", "b", 3), rendezvous("b", "a", 5));

    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| guest.vm.run(&a));
        let b = scope.spawn(|| guest.vm.run(&b));
        (a.join().unwrap().unwrap(), b.join().unwrap().unwrap())
    });

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
        "Remove-Item -LiteralPath C:/ProgramData/devvm/a.flag, C:/ProgramData/devvm/b.flag",
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
        "Remove-Item -LiteralPath C:/ProgramData/devvm/go.flag -ErrorAction Ignore",
    );
    // The command ends only once the host has the first line: the host creates the flag then. The guest's wait has a
    // bound, for a product that does not stream: nothing then tells the host about the first line, and the command
    // ends by itself with a status that fails the test.
    let command = pwsh(
        RunAs::Elevated,
        "[Console]::Out.WriteLine('first'); [Console]::Out.Flush(); \
         $watch = [Diagnostics.Stopwatch]::StartNew(); \
         while (-not (Test-Path -LiteralPath C:/ProgramData/devvm/go.flag)) { \
             if ($watch.Elapsed.TotalSeconds -gt 300) { [Console]::Out.WriteLine('gave up'); exit 9 }; \
             Start-Sleep -Milliseconds 100 }; \
         [Console]::Out.WriteLine('second')",
    );
    let (sender, first) = mpsc::channel();

    let (status, seen) = std::thread::scope(|scope| {
        let run = scope.spawn(|| {
            let mut watch = Watch {
                seen: Vec::new(),
                first: Some(sender),
            };
            let status = guest.vm.run_to(&command, &mut watch, &mut io::sink()).unwrap();
            (status, watch.seen)
        });
        // The first line comes while the command still runs; if the command ends without it, the sender is dropped.
        if first.recv().is_ok() {
            printed(
                &guest.vm,
                "Set-Content -LiteralPath C:/ProgramData/devvm/go.flag -Value x",
            );
        }
        run.join().unwrap()
    });

    let text = String::from_utf8_lossy(&seen);
    assert_eq!(status, ExitStatus::Code(0), "{text}");
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    assert_eq!(
        lines,
        ["first", "second"],
        "the output reached the host while the command ran"
    );
    printed(&guest.vm, "Remove-Item -LiteralPath C:/ProgramData/devvm/go.flag");
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

#[skuld::test]
fn windows_an_elevated_child_ends_when_the_client_goes_away(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    // The command prints its process id, then runs until it is ended.
    let command = pwsh(
        RunAs::Elevated,
        "[Console]::Out.WriteLine($PID); [Console]::Out.Flush(); while ($true) { Start-Sleep -Seconds 1 }",
    );
    let mut leaver = Leaver { seen: Vec::new() };

    // devvm ends the command when its output has nowhere to go; the status that never comes is the guest's.
    let result = guest.vm.run_to(&command, &mut leaver, &mut io::sink());

    let pid = String::from_utf8_lossy(&leaver.seen).trim().to_owned();
    assert!(
        pid.chars().all(|c| c.is_ascii_digit()) && !pid.is_empty(),
        "{pid:?} {result:?}"
    );
    until(|| {
        printed(
            &guest.vm,
            &format!("if (Get-Process -Id {pid} -ErrorAction Ignore) {{ 'running' }} else {{ 'gone' }}"),
        ) == "gone"
    });
    // The runner removes the request of a caller that ended.
    until(|| pending(&guest.vm) == "0");
}

// A request that gets no answer =======================================================================================

#[skuld::test]
fn windows_an_elevated_command_whose_runner_dies_ends_with_an_error_not_a_wait(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    let up = Up::new(&base.image, "w-runner-dies", &windows_options()).unwrap();
    // The command ends the runner that serves it: the process that started `eb-runner.ps1`, never itself.
    let command = pwsh(
        RunAs::Elevated,
        r#"Get-CimInstance Win32_Process -Filter "Name = 'pwsh.exe'" | Where-Object { $_.ProcessId -ne $PID -and $_.CommandLine -match 'eb-runner\.ps1' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }; Start-Sleep -Seconds 60"#,
    );

    let seen = up.run(&command).unwrap();

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
        let up = Up::new(&base.image, &format!("w-task-{}", change.what), &windows_options()).unwrap();
        // The last request the task serves.
        let done = up.run(&pwsh(RunAs::Elevated, &change.script)).unwrap();
        assert_eq!(
            done.status,
            ExitStatus::Code(0),
            "{}: {}",
            change.what,
            done.stderr_lossy()
        );

        let seen = up
            .run(&pwsh(
                RunAs::Elevated,
                "Set-Content -LiteralPath C:/ProgramData/devvm/ran.txt -Value x",
            ))
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
