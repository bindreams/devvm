# The elevated helper's runner (PowerShell 7). The scheduled task starts it once per request, as `user` at High in the
# interactive session, with the request's id as its first argument; it serves that request only. The caller half of the
# contract is `elevate.rs`: a request is a directory under the root holding the script and the caller's identity, and the
# caller waits for the answer in files.
param([string]$Id)
$ErrorActionPreference = 'Stop'
@@!UTF8_OUTPUT@@

$root = @@EB_ROOT@@
$scriptName = @@EB_SCRIPT@@
$callerName = @@EB_CALLER@@
$claimName = @@EB_CLAIM@@
$stdoutName = @@EB_STDOUT@@
$stderrName = @@EB_STDERR@@
$exitName = @@EB_EXIT@@
$doneName = @@EB_DONE@@
$readName = @@EB_READ@@
$pwsh = @@PWSH_EXE@@
$utf8 = [Text.UTF8Encoding]::new($false)
# Paces a wait for a file; it never ends one.
$pause = 200

if ($Id -notmatch '^[0-9a-f]{32}$') { exit 2 }

# The native helper, compiled once by the bootstrap: a caller opened by one handle, and a child started in a job of its
# own (Devvm.cs). Ending the child's tree is ending that job.
Add-Type -LiteralPath (Join-Path $PSScriptRoot @@NATIVE_DLL@@)

# Whether a request's caller has ended: its process is gone, or the process of that id started at another time. A caller
# that cannot be told is not ended, so its request stays.
function Test-CallerEnded([string]$Dir) {
    try { $text = [IO.File]::ReadAllText((Join-Path $Dir $callerName)) } catch { return $true }
    if ($text -notmatch '^\s*(\d+)\s+(\d+)\s*$') { return $true }
    try {
        $caller = [Devvm.Caller]::Open([int]$Matches[1], [long]$Matches[2])
    } catch { return $false }
    if ($null -eq $caller) { return $true }
    $caller.Dispose()
    return $false
}

# A claim file that exists is held by a live runner: it is created with CreateNew and shared with no one, and the OS
# releases it when its runner dies, however it dies.
function Test-Held([string]$Path) {
    try { [IO.File]::Open($Path, 'Open', 'ReadWrite', 'None').Dispose(); return $false }
    catch [IO.FileNotFoundException] { return $false }
    catch [IO.IOException] { return $true }
    catch [UnauthorizedAccessException] { return $true }
}

# Requests whose caller has ended and that no runner holds. A request another runner or its caller removes first is
# not an error, and one that stays is the next runner's, or the boot task's.
foreach ($other in [IO.Directory]::GetDirectories($root)) {
    $name = [IO.Path]::GetFileName($other)
    if ($name -eq $Id -or $name.EndsWith('.tmp')) { continue }
    if (-not (Test-CallerEnded $other) -or (Test-Held (Join-Path $other $claimName))) { continue }
    try { Remove-Item -LiteralPath $other -Recurse -Force } catch { }
}

$dir = Join-Path $root $Id
# This start's own request: served by whoever creates its claim. A request without a directory is withdrawn or swept; one
# with a claim is a runner's.
$claim = $null
try { $claim = [IO.File]::Open((Join-Path $dir $claimName), 'CreateNew', 'Write', 'None') } catch [IO.IOException] { }
if ($null -eq $claim) { exit 0 }

$exitCode = 126
$failure = $null
$callerGone = $false
$callerExit = $null
$out = $null
$err = $null
$caller = $null
$child = $null
try {
    # Parsed again: the sweep above skipped it.
    $text = [IO.File]::ReadAllText((Join-Path $dir $callerName))
    if ($text -notmatch '^\s*(\d+)\s+(\d+)\s*$') { throw 'the request names no caller' }
    # One handle on the caller: its start time is checked on it and its end is awaited on it.
    $caller = [Devvm.Caller]::Open([int]$Matches[1], [long]$Matches[2])
    if ($null -eq $caller) {
        $callerExit = [Threading.Tasks.Task]::CompletedTask
        $callerGone = $true
    } else {
        $callerExit = $caller.Exited
        # No buffering: the caller reads the files while they grow.
        $out = [IO.FileStream]::new((Join-Path $dir $stdoutName), 'Create', 'Write', 'Read', 1)
        $err = [IO.FileStream]::new((Join-Path $dir $stderrName), 'Create', 'Write', 'Read', 1)
        # devvm's exec sends no input: the command's standard input is at end of file.
        $child = [Devvm.Child]::Start($pwsh, '"' + $pwsh + '" -NoProfile -NonInteractive -File "' + (Join-Path $dir $scriptName) + '"')
        $finished = [Threading.Tasks.Task]::WhenAll([Threading.Tasks.Task[]]@(
            $child.Output.CopyToAsync($out),
            $child.Error.CopyToAsync($err),
            $child.Exited))
        if ([Threading.Tasks.Task]::WaitAny([Threading.Tasks.Task[]]@($finished, $callerExit)) -ne 0) {
            $callerGone = $true
            $child.Terminate()
        }
        $finished.Wait()
        if (-not $callerGone) { $exitCode = $child.ExitCode }
    }
} catch {
    $failure = "devvm: the elevated runner failed: $($_.Exception.Message)"
    # No caller to answer: the request goes.
    if ($null -eq $callerExit) { $callerGone = $true }
    if ($null -ne $child) { try { $child.Terminate() } catch { } }
} finally {
    foreach ($stream in $out, $err) { if ($null -ne $stream) { $stream.Dispose() } }
    # The job goes with its handle: whatever the command left running ends with it.
    if ($null -ne $child) { $child.Dispose() }
}

if (-not $callerGone) {
    if ($failure) { [IO.File]::AppendAllText((Join-Path $dir $stderrName), $failure + "`n", $utf8) }
    # The status, then done: the caller reads the status only once done exists.
    [IO.File]::WriteAllText((Join-Path $dir $exitName), $exitCode.ToString([Globalization.CultureInfo]::InvariantCulture), $utf8)
    [IO.File]::WriteAllText((Join-Path $dir $doneName), '', $utf8)
    # The caller says when it has read everything; a caller that ends first is as good. Neither wait has a time bound.
    while (-not [IO.File]::Exists((Join-Path $dir $readName)) -and -not $callerExit.IsCompleted) { Start-Sleep -Milliseconds $pause }
}
if ($null -ne $caller) { $caller.Dispose() }
# The last act: the request goes. The claim is closed first, or it would keep its own directory.
$claim.Dispose()
try { Remove-Item -LiteralPath $dir -Recurse -Force } catch { }
