//! Running a command at High: the caller half of the elevated helper (plan 6.4, "E-B").
//!
//! The Medium session cannot become High, so it hands the command to a scheduled task of `user` that runs with the
//! highest privileges and waits for its answer in files. One start serves one request, tasks run in parallel, and
//! nobody has to be at the keyboard: the task is registered by the base's bootstrap and the task's own runner
//! (`eb-runner.ps1`, a template of the base) is the other half of this contract.
//!
//! A request is a directory `<ROOT>\<id>` made complete under the name `<id>.tmp` and renamed, holding
//! [`SCRIPT`] (the script a Medium wrapper would run for the same command, which the runner starts with
//! `pwsh -NoProfile -NonInteractive -File`, its standard input at end of file) and [`CALLER`] (`<pid> <start time in
//! UTC ticks>` of the process waiting: the runner ends the command's process tree when that process ends). The runner
//! **claims** the oldest request that has no [`CLAIM`] by creating `CLAIM` with `CreateNew` and sharing none, and holds
//! it until it exits; it writes [`STDOUT`] and [`STDERR`] as the command produces output, opened with
//! `FileShare.Read` at least (the caller reads them while they grow), then [`EXIT`] (the status as a signed decimal)
//! and, last, [`DONE`]. A `<id>.tmp` directory is not a request yet. The directory is the caller's to remove; the boot
//! task sweeps what a killed caller left.
//!
//! The caller's wait ends from file state alone, with no time bound: a held claim is a live runner; a released claim
//! with no `DONE` is a runner that died; no claim and no running instance of the task is a helper that is not going to
//! run the command (the caller claims the request itself, so no runner can take it afterwards).

use super::wrap::{SESSION_CHECK, literal};

/// Where the requests are: `Users` may modify it, so a request needs no elevated writer.
pub(super) const ROOT: &str = r"C:\ProgramData\devvm\eb";
/// The scheduled task that runs a request: `user`, highest privileges, `MultipleInstances Parallel`.
pub(super) const TASK: &str = "devvm-eb";
pub(super) const SCRIPT: &str = "script.ps1";
pub(super) const CALLER: &str = "caller";
pub(super) const CLAIM: &str = "claim";
pub(super) const STDOUT: &str = "stdout";
pub(super) const STDERR: &str = "stderr";
pub(super) const EXIT: &str = "exit";
pub(super) const DONE: &str = "done";

/// The pause between two looks at the request's files. It paces the wait; it never ends it.
const PAUSE_MILLISECONDS: u32 = 200;

const CALLER_SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
{SESSION_CHECK}$root = {ROOT}
$id = [guid]::NewGuid().ToString('N')
$dir = Join-Path $root $id
$building = "$dir.tmp"
[void][IO.Directory]::CreateDirectory($building)
$utf8 = [Text.UTF8Encoding]::new($false)
[IO.File]::WriteAllText("$building\{SCRIPT}", {INNER}, $utf8)
$self = [Diagnostics.Process]::GetCurrentProcess()
[IO.File]::WriteAllText("$building\{CALLER}", "$($self.Id) $($self.StartTime.ToUniversalTime().Ticks)", $utf8)
[IO.Directory]::Move($building, $dir)
function Fail([string]$message) {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
    [Console]::Error.WriteLine("devvm: $message")
    exit 126
}
$states = 'unknown', 'disabled', 'queued', 'ready', 'running'
$service = New-Object -ComObject 'Schedule.Service'
$service.Connect()
$task = $service.GetFolder('\').GetTask({TASK})
try { [void]$task.Run($null) } catch { Fail "the elevated helper did not start: $($_.Exception.Message)" }
$claim = Join-Path $dir '{CLAIM}'
$done = Join-Path $dir '{DONE}'
$stdout = [Console]::OpenStandardOutput()
$stderr = [Console]::OpenStandardError()
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
while ($true) {
    $claimed = [IO.File]::Exists($claim)
    $held = $claimed -and (Test-Held $claim)
    $finished = [IO.File]::Exists($done)
    Copy-Out {STDOUT} $stdout
    Copy-Out {STDERR} $stderr
    if ($finished) { break }
    if ($claimed -and -not $held) { Fail 'the elevated runner ended without a result' }
    if (-not $claimed -and $task.GetInstances(0).Count -eq 0) {
        try { [IO.File]::Open($claim, 'CreateNew', 'Write', 'None').Dispose() }
        catch [IO.IOException] { Start-Sleep -Milliseconds {PAUSE}; continue }
        Fail "the elevated helper did not run the command (task state: $($states[[int]$task.State]))"
    }
    Start-Sleep -Milliseconds {PAUSE}
}
$code = [int]([IO.File]::ReadAllText((Join-Path $dir '{EXIT}')).Trim())
foreach ($stream in $streams.Values) { $stream.Dispose() }
Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
exit $code
"#;

/// The script that runs `inner` (the script of the same command for a Medium session) at High and plays the result back.
pub(super) fn caller_script(inner: &str) -> String {
    CALLER_SCRIPT
        .replace("{SESSION_CHECK}", SESSION_CHECK)
        .replace("{ROOT}", &literal(ROOT))
        .replace("{SCRIPT}", SCRIPT)
        .replace("{CALLER}", CALLER)
        .replace("{TASK}", &literal(TASK))
        .replace("{CLAIM}", CLAIM)
        .replace("{DONE}", DONE)
        .replace("{STDOUT}", &literal(STDOUT))
        .replace("{STDERR}", &literal(STDERR))
        .replace("{EXIT}", EXIT)
        .replace("{PAUSE}", &PAUSE_MILLISECONDS.to_string())
        // Last: the user's text is never scanned for a placeholder.
        .replace("{INNER}", &literal(inner))
}
