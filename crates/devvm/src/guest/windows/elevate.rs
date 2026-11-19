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
//!   that id started at another time), and every file or link in the root, then **claims** its own by creating
//!   [`CLAIM`] with `CreateNew` and sharing none, and holds it until it exits. A request that has a claim, or is gone, is
//!   not served;
//! - it ends the command's process tree when the caller ends, writes [`STDOUT`] and [`STDERR`] as the command produces
//!   output (opened with `FileShare.Read` at least: the caller reads them while they grow), then [`EXIT`] (the status as
//!   a signed decimal) and, last, [`DONE`];
//! - it removes its request directory as its last act: after the caller has written [`READ`] or, if the caller ends
//!   first, at once. It waits for neither with a time bound.
//!
//! The caller removes a request only when no runner will touch it: one it withdrew or one whose runner died. What a
//! killed caller leaves is the runner's to remove, and the boot task removes what nothing else did (every entry of the
//! root, tried once).
//!
//! **A request is removed by one function.** Every remover calls `Remove-Request` ([`REMOVE_REQUEST`]): the order of its
//! steps says who a half-removed request belongs to; the caller and the runner's end retry until it is gone, the sweep
//! and the boot task try once.
//!
//! **A link in the root is removed as a link.** A request, or an entry of one, that is already a junction or a symbolic
//! link when a remover reaches it is removed as the link and never followed. A path whose attributes cannot be read, for
//! any reason but that it is not there, is taken for a link. A link swapped in while a removal runs can be followed: the
//! root's writers are trusted (see [`ROOT`]).
//!
//! The sweep also takes an `<id>.tmp` whose caller has ended; one with no `caller` file stays for the boot task.
//!
//! The caller's wait ends from file state alone, with no time bound: a held claim is a live runner; a released claim
//! with no `DONE` is a runner that died; no claim and an instance, the one `Run` returned for this request, that is no
//! longer queued or running is a helper that is not going to run the command (the caller claims the request itself, so
//! no runner can take it afterwards). An instance whose state cannot be read is the same: the caller withdraws the
//! request, and the wait goes on if a runner has claimed it. Another request's instance does not count: the task runs
//! them in parallel.

use super::wrap::{literal, session_check};

#[cfg(test)]
#[path = "elevate_tests.rs"]
mod elevate_tests;

#[cfg(all(test, windows))]
#[path = "elevate_pwsh_tests.rs"]
mod elevate_pwsh_tests;

/// Where the requests are: `Users` may modify it, so a request needs no elevated writer. The ACL gives no protection
/// against the accounts that can write it: `user`, an administrator, and the guest's own service accounts (`Users` also
/// holds Authenticated Users and INTERACTIVE), which are the guest OS.
pub(crate) const ROOT: &str = r"C:\ProgramData\devvm\eb";
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
pub(super) const PAUSE_MILLISECONDS: u32 = 200;

/// The PowerShell functions `Remove-Request`, `Remove-Entry`, `Test-Link`, `Test-RemovalPending` and `Get-Verbatim`, the
/// same text in the caller script, the runner and the boot task. `Remove-Request` returns when the path is gone (a
/// request's directory, or a stray file). It removes every file but `caller` and `claim`, then `caller`, then `claim`,
/// then the directory; a try that fails because a file is open elsewhere (sharing violation 32, lock violation 33,
/// directory not empty 145), or because an entry is already gone, is repeated from the top after a pause; with `-Once` it
/// returns instead, and what is left is the next sweep's. Any other error (access denied included: measured permanent,
/// not transient) is thrown.
///
/// Every path is verbatim: `Get-Verbatim` puts `\\?\` on it, and a listing under a verbatim path returns verbatim
/// children. An entry whose name Win32 would change (a trailing dot or space, a device name such as `con`) is then the
/// entry that is named, and `[IO.File]` and `[IO.Directory]` calls remove it.
///
/// A reparse point is looked at without being followed (`Test-Link`, which reads any path it cannot read as a link): one
/// that is the path itself, or an entry, is removed as the link (`Remove-Entry`), and nothing under it is listed or
/// deleted. An entry is deleted without recursion after its read-only attribute is cleared, so a directory that got a
/// child meanwhile (a late runner's `claim`) fails with 145 and is retried.
///
/// Every intermediate state is safe: `script` goes first, so a half-removed request never runs a command, and `caller`
/// and `claim` go last, so the sweep and a late runner can still tell whose it is.
const REMOVE_REQUEST: &str = r#"function Test-RemovalPending($failure) {
    if ($failure -is [Management.Automation.MethodInvocationException] -and $failure.InnerException) { $failure = $failure.InnerException }
    if ($failure -is [Management.Automation.ItemNotFoundException] -or $failure -is [IO.FileNotFoundException] -or $failure -is [IO.DirectoryNotFoundException]) { return $true }
    return ($failure -is [IO.IOException]) -and (($failure.HResult -band 0xFFFF) -in 32, 33, 145)
}
function Get-Verbatim([string]$path) {
    if ($path.StartsWith('\\?\')) { return $path }
    return '\\?\' + $path
}
function Test-Link([string]$path) {
    try { return ([IO.File]::GetAttributes($path) -band [IO.FileAttributes]::ReparsePoint) -ne 0 }
    catch [IO.FileNotFoundException], [IO.DirectoryNotFoundException] { return $false }
    catch { return $true }
}
function Remove-Entry([string]$entry) {
    $link = Test-Link $entry
    $directory = [IO.Directory]::Exists($entry)
    if ($directory -and -not $link) {
        foreach ($child in [IO.Directory]::GetFileSystemEntries($entry)) { Remove-Entry $child }
    }
    [IO.File]::SetAttributes($entry, [IO.File]::GetAttributes($entry) -band -bnot [IO.FileAttributes]::ReadOnly)
    if ($directory) { [IO.Directory]::Delete($entry) }
    else { [IO.File]::Delete($entry) }
}
function Remove-Request([string]$path, [switch]$Once) {
    $path = Get-Verbatim $path
    $last = [IO.Path]::Combine($path, {CALLER}), [IO.Path]::Combine($path, {CLAIM})
    while ([IO.File]::Exists($path) -or [IO.Directory]::Exists($path)) {
        try {
            if ((Test-Link $path) -or -not [IO.Directory]::Exists($path)) { Remove-Entry $path }
            else {
                $first = @([IO.Directory]::GetFileSystemEntries($path) | Where-Object { [IO.Path]::GetFileName($_) -notin {CALLER}, {CLAIM} })
                foreach ($entry in $first + $last + $path) {
                    if ([IO.File]::Exists($entry) -or [IO.Directory]::Exists($entry)) { Remove-Entry $entry }
                }
            }
        }
        catch {
            if (-not (Test-RemovalPending $_.Exception)) { throw }
            if ($Once) { return }
            if ([IO.File]::Exists($path) -or [IO.Directory]::Exists($path)) { Start-Sleep -Milliseconds {PAUSE} }
        }
    }
}
"#;

/// [`REMOVE_REQUEST`] with its file names and its pause filled in.
pub(super) fn remove_request() -> String {
    REMOVE_REQUEST
        .replace("{CALLER}", &literal(CALLER))
        .replace("{CLAIM}", &literal(CLAIM))
        .replace("{PAUSE}", &PAUSE_MILLISECONDS.to_string())
}

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
{REMOVE_REQUEST}function Warn([string]$what, $failure) {
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
# `SCHED_E_TASK_NOT_RUNNING` (0x8004130B): what `Refresh` of an ended instance throws, anywhere in the exception chain
# (measured on a guest).
function Test-NotRunning($failure) {
    for ($e = $failure; $e; $e = $e.InnerException) {
        if ($e -is [Runtime.InteropServices.COMException] -and $e.HResult -eq 0x8004130B) { return $true }
    }
    return $false
}
# The state of the instance this caller started, as text (the task runs other requests' instances at the same time, so
# the task's own state says nothing). `not started` is no instance, `not running` an ended one (see Test-NotRunning). A
# state that cannot be read is returned as the exception, never as a state; nothing here throws. `State` reads stale 0
# after a throwing `Refresh`, and PowerShell answers a failed getter with `$null`, so `$null` means unreadable.
function Get-InstanceState($instance) {
    if ($null -eq $instance) { return 'not started' }
    try {
        $instance.Refresh()
        $raw = $instance.State
        if ($null -eq $raw) { return [InvalidOperationException]::new('the state read as empty') }
        $state = [int]$raw
    } catch {
        if (Test-NotRunning $_.Exception) { return 'not running' }
        return $_.Exception
    }
    if ($state -ge 0 -and $state -lt $states.Count) { return $states[$state] }
    return "$state"
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
    $instance = $null
    try { $instance = $task.Run($id) }
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
        if (-not $claimed) {
            $instanceState = Get-InstanceState $instance
            $unreadable = $instanceState -is [Exception]
            if ($unreadable -or ($instanceState -ne 'queued' -and $instanceState -ne 'running')) {
                # A runner that claimed since the look serves the request: the wait goes on.
                if (-not (Withdraw-Request)) { Start-Sleep -Milliseconds {PAUSE}; continue }
                $message = if ($unreadable) { "the elevated helper failed: the state of the instance could not be read: $($instanceState.Message)" }
                else { "the elevated helper did not run the command (instance state: $instanceState)" }
                $abandoned = $true
                break
            }
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
            try { Remove-Request $building } catch { Warn 'could not remove the unpublished request' $_ }
        }
    } elseif ([IO.Directory]::Exists($dir)) {
        if ($abandoned) {
            try { Remove-Request $dir } catch { Warn 'could not remove the withdrawn request' $_ }
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
        .replace("{REMOVE_REQUEST}", &remove_request())
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
