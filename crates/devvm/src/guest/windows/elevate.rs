//! Running a command at High: the caller half of the elevated helper.
//!
//! The Medium session cannot become High, so it hands the command to a scheduled task of `user` that runs with the
//! highest privileges and waits for its answer in files. The task is registered by the base's bootstrap, and its own
//! runner (`eb-runner.ps1`, a template of the base) is the other half of this contract.
//!
//! **The task** is `user`'s, registered with the principal's logon type `InteractiveToken` and the highest run level
//! (not S4U, "whether the user is logged on or not"): the runner runs in the user's interactive session at High. Every
//! script it runs, an elevated one included, begins with [`session_check`] and ends with 126 in session 0, so a runner
//! that does not run in that session answers every command with 126. Tasks run in parallel, and nobody has to be at
//! the keyboard.
//!
//! **A request** is a directory `<ROOT>\<id>` made complete under the name `<id>.tmp` and renamed, so a request is
//! published last. It holds [`SCRIPT`] (the script a Medium wrapper would run for the same command, which the runner
//! starts with `pwsh -NoProfile -NonInteractive -File`, its standard input at end of file) and [`CALLER`] (`<pid> <start
//! time in UTC ticks>` of the process waiting). A `<id>.tmp` directory is not a request.
//!
//! **A start serves one request.** The caller starts the task with the request's id (`Run($id)`, the task's `$(Arg0)`),
//! and the runner serves that request only:
//!
//! - it removes every request whose caller has ended and that no runner holds (its process is gone, or the process of
//!   that id started at another time), then **claims** its own by creating [`CLAIM`] with `CreateNew` and sharing none,
//!   and holds it until it exits. A request that has a claim, or is gone, is not served;
//! - it ends the command's process tree when the caller ends, writes [`STDOUT`] and [`STDERR`] as the command produces
//!   output (opened with `FileShare.Read` at least: the caller reads them while they grow), then [`EXIT`] (the status as
//!   a signed decimal) and, last, [`DONE`];
//! - it removes its request directory as its last act: after the caller has written [`READ`] or, if the caller ends
//!   first, at once. It waits for neither with a time bound.
//!
//! The caller removes a request only when no runner will touch it: one it withdrew or one whose runner died. What a
//! killed caller leaves is the runner's to remove, and the boot task sweeps what nothing else did.
//!
//! The caller's wait ends from file state alone, with no time bound: a held claim is a live runner; a released claim
//! with no `DONE` is a runner that died; no claim and no running instance of the task is a helper that is not going to
//! run the command (the caller claims the request itself, so no runner can take it afterwards).

use super::wrap::{literal, session_check};

#[cfg(test)]
#[path = "elevate_tests.rs"]
mod elevate_tests;

#[cfg(all(test, windows))]
#[path = "elevate_pwsh_tests.rs"]
mod elevate_pwsh_tests;

/// Where the requests are: `Users` may modify it, so a request needs no elevated writer.
pub(super) const ROOT: &str = r"C:\ProgramData\devvm\eb";
/// The scheduled task that runs a request: `user`, highest privileges, `MultipleInstances Parallel`.
pub(crate) const TASK: &str = "devvm-eb";
pub(super) const SCRIPT: &str = "script.ps1";
pub(super) const CALLER: &str = "caller";
pub(super) const CLAIM: &str = "claim";
pub(super) const STDOUT: &str = "stdout";
pub(super) const STDERR: &str = "stderr";
pub(super) const EXIT: &str = "exit";
pub(super) const DONE: &str = "done";
/// Written by the caller when it has read everything the runner wrote: the runner may remove the request.
pub(super) const READ: &str = "read";

/// The pause between two looks at the request's files. It paces the wait; it never ends it.
const PAUSE_MILLISECONDS: u32 = 200;

/// The task is looked up before anything is made. Building and publishing the request, the start and the wait are one
/// `try`: a failure in it is an answer of 126 that names it, and a published request is withdrawn if no runner holds it.
/// The `finally` releases what the caller opened and removes the request when nobody else will touch it. Failures of
/// that cleanup are warnings on stderr, never the command's status.
const CALLER_SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
{SESSION_CHECK}$root = {ROOT}
$utf8 = [Text.UTF8Encoding]::new($false)
try {
    $service = New-Object -ComObject 'Schedule.Service'
    $service.Connect()
    $task = $service.GetFolder('\').GetTask({TASK})
} catch {
    [Console]::Error.WriteLine("devvm: the elevated helper is not available: $($_.Exception.Message)")
    exit 126
}
$states = 'unknown', 'disabled', 'queued', 'ready', 'running'
$self = [Diagnostics.Process]::GetCurrentProcess()
$id = [guid]::NewGuid().ToString('N')
$dir = Join-Path $root $id
$building = "$dir.tmp"
$claim = Join-Path $dir '{CLAIM}'
$done = Join-Path $dir '{DONE}'
$streams = @{}
function Copy-Out([string]$name, $sink) {
    if (-not $streams.ContainsKey($name)) {
        $path = Join-Path $dir $name
        if (-not [IO.File]::Exists($path)) { return }
        $streams[$name] = [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite, Delete')
    }
    $streams[$name].CopyTo($sink)
    $sink.Flush()
}
function Test-Held([string]$path) {
    try { [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite, Delete').Dispose(); return $false }
    catch [IO.FileNotFoundException] { return $false }
    catch [IO.IOException] { return $true }
}
function Warn([string]$what, $failure) {
    [Console]::Error.WriteLine('devvm: warning: ' + $what + ': ' + $failure.Exception.Message)
}
# Claiming the request ourselves withdraws it: no runner takes it. False when the claim is a runner's, which serves the
# request. Any other failure is thrown.
function Withdraw-Request {
    try { [IO.File]::Open($claim, 'CreateNew', 'Write', 'None').Dispose(); return $true }
    catch [IO.IOException] {
        if ([IO.File]::Exists($claim)) { return $false }
        throw
    }
}
$code = 126
$message = $null
$abandoned = $false
$published = $false
try {
    [void][IO.Directory]::CreateDirectory($building)
    [IO.File]::WriteAllText("$building\{SCRIPT}", {INNER}, $utf8)
    [IO.File]::WriteAllText("$building\{CALLER}", "$($self.Id) $($self.StartTime.ToUniversalTime().Ticks)", $utf8)
    [IO.Directory]::Move($building, $dir)
    $published = $true
    $stdout = [Console]::OpenStandardOutput()
    $stderr = [Console]::OpenStandardError()
    try { [void]$task.Run($id) }
    catch {
        $message = "the elevated helper did not start: $($_.Exception.Message)"
        if (Withdraw-Request) { $abandoned = $true } else { $message = $null }
    }
    while (-not $message) {
        if (-not [IO.Directory]::Exists($dir)) { $message = 'the elevated request was removed'; break }
        $claimed = [IO.File]::Exists($claim)
        $held = $claimed -and (Test-Held $claim)
        $finished = [IO.File]::Exists($done)
        Copy-Out {STDOUT} $stdout
        Copy-Out {STDERR} $stderr
        if ($finished) {
            $status = [IO.File]::ReadAllText((Join-Path $dir '{EXIT}')).Trim()
            if (-not [int]::TryParse($status, [Globalization.NumberStyles]::Integer, [Globalization.CultureInfo]::InvariantCulture, [ref]$code)) {
                $message = 'the elevated runner wrote an unreadable status'
            }
            break
        }
        if ($claimed -and -not $held) { $message = 'the elevated runner ended without a result'; $abandoned = $true; break }
        if (-not $claimed -and $task.GetInstances(0).Count -eq 0) {
            if (-not (Withdraw-Request)) { Start-Sleep -Milliseconds {PAUSE}; continue }
            $message = "the elevated helper did not run the command (task state: $($states[[int]$task.State]))"
            $abandoned = $true
            break
        }
        Start-Sleep -Milliseconds {PAUSE}
    }
} catch {
    if ($published) {
        $message = "the elevated helper failed: $($_.Exception.Message)"
        if (-not $abandoned) {
            try { $abandoned = Withdraw-Request } catch { Warn 'could not withdraw the request' $_ }
        }
    } else {
        $message = "the elevated helper could not make the request: $($_.Exception.Message)"
    }
} finally {
    foreach ($stream in $streams.Values) { $stream.Dispose() }
    if (-not $published) {
        if ([IO.Directory]::Exists($building)) {
            try { Remove-Item -LiteralPath $building -Recurse -Force } catch { Warn 'could not remove the unpublished request' $_ }
        }
    } elseif ([IO.Directory]::Exists($dir)) {
        if ($abandoned) {
            # Out of the requests' names first, so that no runner finds half of it.
            $leaving = $dir
            try { [IO.Directory]::Move($dir, $building); $leaving = $building } catch { Warn 'could not rename the withdrawn request' $_ }
            try { Remove-Item -LiteralPath $leaving -Recurse -Force } catch { Warn 'could not remove the withdrawn request' $_ }
        } else {
            try { [IO.File]::WriteAllText((Join-Path $dir '{READ}'), '', $utf8) } catch { Warn 'could not tell the runner the request was read' $_ }
        }
    }
}
if ($message) { [Console]::Error.WriteLine("devvm: $message"); exit 126 }
exit $code
"#;

/// The script that runs `inner` (the script of the same command for a Medium session) at High and plays the result back.
pub(super) fn caller_script(inner: &str) -> String {
    CALLER_SCRIPT
        .replace("{SESSION_CHECK}", &session_check())
        .replace("{ROOT}", &literal(ROOT))
        .replace("{SCRIPT}", SCRIPT)
        .replace("{CALLER}", CALLER)
        .replace("{TASK}", &literal(TASK))
        .replace("{CLAIM}", CLAIM)
        .replace("{DONE}", DONE)
        .replace("{READ}", READ)
        .replace("{STDOUT}", &literal(STDOUT))
        .replace("{STDERR}", &literal(STDERR))
        .replace("{EXIT}", EXIT)
        .replace("{PAUSE}", &PAUSE_MILLISECONDS.to_string())
        // Last: the user's text is never scanned for a placeholder.
        .replace("{INNER}", &literal(inner))
}
