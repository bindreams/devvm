//! A measurement, not a check: what the Task Scheduler says of the helper task's instance (`IRunningTask`) from the
//! moment `Run` returns until the instance ends, which the caller script of an elevated command (`guest/windows/
//! elevate.rs`) reads to tell a helper that will not run the command. Each distinct observation is one `M:` line of the
//! log. The test asserts only that the instance ended.

use std::io::{self, Write};

use super::{SharedWindowsGuest, pwsh, shared_windows_guest};
use crate::guest::windows::elevate::{CALLER, CLAIM, DONE, READ, ROOT, SCRIPT, TASK};
use crate::vm_suite::waits::{answer_to, step};
use crate::{ExitStatus, RunAs};

/// The guest side. `$live` is set before it: a request that a runner serves, or an id that no request has (the runner
/// exits without claiming). The loop is tight, has no pause and no count: it ends when the instance does (its state is
/// ready or disabled, or its `InstanceGuid` is no longer among the task's instances), and the host's [`answer_to`] bound
/// ends a run in which it never does. A `Refresh` that throws is recorded, and the loop goes on. The times are for
/// reading the log, not for deciding.
const MEASUREMENT: &str = r#"$ErrorActionPreference = 'Stop'
$service = New-Object -ComObject 'Schedule.Service'
$service.Connect()
$task = $service.GetFolder('\').GetTask('{TASK}')
$root = '{ROOT}'
$utf8 = [Text.UTF8Encoding]::new($false)
$id = [guid]::NewGuid().ToString('N')
$dir = Join-Path $root $id
function Chain($e) {
    $parts = @()
    for (; $e; $e = $e.InnerException) {
        $parts += ('{0} 0x{1:X8} "{2}"' -f $e.GetType().FullName, $e.HResult, $e.Message.Trim())
    }
    $parts -join ' <- '
}
# One look: Refresh (a throw is recorded; only SCHED_E_TASK_NOT_RUNNING ends), then the raw State (the error it leaves when it reads as $null),
# then the task's instances. Why says what ended the instance, and is $null while it has not ended.
function Look {
    $refresh = 'no instance'
    $state = 'no instance'
    $raw = $null
    $why = $null
    if ($null -ne $instance) {
        $refresh = 'ok'
        try { $instance.Refresh() } catch {
            $refresh = 'threw ' + (Chain $_.Exception)
            if ($_.Exception.HResult -eq 0x8004130B -or $_.Exception.InnerException.HResult -eq 0x8004130B) { $why = 'Refresh: SCHED_E_TASK_NOT_RUNNING' }
        }
        $ErrorActionPreference = 'Continue'
        $Error.Clear()
        try {
            $raw = $instance.State
            $state = if ($null -eq $raw) { '$null' } else { "$raw ($($raw.GetType().Name))" }
            if ($null -eq $raw -and $Error.Count -gt 0) { $state += ' after the error ' + (Chain $Error[0].Exception) }
        } catch { $state = 'threw ' + (Chain $_.Exception) }
        if ($raw -in 1, 3) { $why = "state $raw" }
    }
    $listed = 'unknown'
    try {
        $guids = @($task.GetInstances(0) | ForEach-Object { $_.InstanceGuid })
        $instances = "$($guids.Count) [$($guids -join ', ')]"
        if ($guid) { $listed = [string]($guids -contains $guid) }
        if ($listed -eq 'True') { $script:seen = $true }
        # Only an instance once listed can be no longer listed: right after Run it may not be listed yet.
        if ($script:seen -and $listed -eq 'False' -and -not $why) { $why = 'InstanceGuid no longer among the instances' }
        if (-not $guid -and $guids.Count -eq 0 -and -not $why) { $why = 'no instances' }
    } catch { $instances = 'threw ' + (Chain $_.Exception) }
    $files = 'request=' + [IO.Directory]::Exists($dir) + ' claim=' + [IO.File]::Exists("$dir\{CLAIM}") + ' done=' + [IO.File]::Exists("$dir\{DONE}")
    [pscustomobject]@{ Key = "Refresh: $refresh; State: $state; GetInstances(0): $instances; listed: $listed; $files"; Why = $why }
}
if ($live) {
    $self = [Diagnostics.Process]::GetCurrentProcess()
    $building = "$dir.tmp"
    [void][IO.Directory]::CreateDirectory($building)
    [IO.File]::WriteAllText("$building\{SCRIPT}", 'exit 0', $utf8)
    [IO.File]::WriteAllText("$building\{CALLER}","$($self.Id) $($self.StartTime.ToUniversalTime().Ticks)", $utf8)
    [IO.Directory]::Move($building, $dir)
}
$clock = [Diagnostics.Stopwatch]::StartNew()
$instance = $task.Run($id)
$returned = if ($null -eq $instance) { 'null' } else { $instance.GetType().FullName }
$guid = $null
$script:seen = $false
if ($null -ne $instance) {
    try { $g = $instance.InstanceGuid; $guid = if ($g) { "$g" } else { $null } } catch { $guid = $null; $returned += ' (InstanceGuid threw ' + (Chain $_.Exception) + ')' }
}
'{0,6} ms  Run({1}) returned {2}; InstanceGuid={3}; live={4}' -f $clock.ElapsedMilliseconds, $id, $returned, $guid, $live
$last = $null
$wrote = $false
do {
    $look = Look
    if ($look.Key -ne $last) { '{0,6} ms  {1}' -f $clock.ElapsedMilliseconds, $look.Key; $last = $look.Key }
    # The runner waits for the caller's word that it read the answer: the instance runs until then.
    if ($live -and -not $wrote -and [IO.File]::Exists("$dir\{DONE}")) {
        [IO.File]::WriteAllText("$dir\{READ}",'', $utf8)
        $wrote = $true
        '{0,6} ms  the caller wrote read' -f $clock.ElapsedMilliseconds
    }
} until ($look.Why)
'{0,6} ms  ended by {1}; request directory exists={2}' -f $clock.ElapsedMilliseconds, $look.Why, [IO.Directory]::Exists($dir)
exit 0"#;

/// Prints each complete line it is written as an `M:` line of the log, as it comes: a measurement that never ends still
/// shows what it saw. It keeps the lines for the test.
struct Lines {
    prefix: &'static str,
    partial: Vec<u8>,
    all: Vec<String>,
}

impl Lines {
    fn new(prefix: &'static str) -> Self {
        Self {
            prefix,
            partial: Vec::new(),
            all: Vec::new(),
        }
    }

    fn take(&mut self, line: &[u8]) {
        let line = String::from_utf8_lossy(line).trim_end().to_owned();
        eprintln!("{}{line}", self.prefix);
        self.all.push(line);
    }

    /// The last line, which has no line end.
    fn finish(&mut self) {
        if !self.partial.is_empty() {
            let rest = std::mem::take(&mut self.partial);
            self.take(&rest);
        }
    }
}

impl Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.partial.extend_from_slice(bytes);
        while let Some(end) = self.partial.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            self.take(&line);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[skuld::test]
fn windows_the_helper_instance_is_measured_from_the_start_to_its_end(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    for (phase, live) in [
        ("a start with an id that no request has", false),
        ("a start with a request that a runner serves", true),
    ] {
        step(&format!("measuring {phase}"));
        eprintln!("M: {phase}");
        let script = [
            ("{TASK}", TASK),
            ("{ROOT}", ROOT),
            ("{SCRIPT}", SCRIPT),
            ("{CALLER}", CALLER),
            ("{CLAIM}", CLAIM),
            ("{DONE}", DONE),
            ("{READ}", READ),
        ]
        .into_iter()
        .fold(MEASUREMENT.to_owned(), |text, (from, to)| text.replace(from, to));
        let command = pwsh(RunAs::User, &format!("$live = ${live}\n{script}"));
        let (mut out, mut err) = (Lines::new("M: "), Lines::new("M: stderr: "));

        let guard = answer_to(&format!("the measurement of {phase}"));
        let status = guest.vm.run_to(&command, &mut out, &mut err);
        drop(guard);
        out.finish();
        err.finish();

        assert_eq!(
            status.as_ref().ok(),
            Some(&ExitStatus::Code(0)),
            "{phase}: {status:?}, stderr {:?}",
            err.all
        );
        assert!(
            out.all.iter().any(|line| line.contains("ended by ")),
            "{phase}: the instance did not end: {:?}",
            out.all
        );
    }
}
