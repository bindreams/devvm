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

# The runner joins a job object of its own (kill on close) before it starts anything, so that every process of the
# command is born in it; when the caller ends, KillOthers ends them all and leaves the runner to clean up.
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Threading;

namespace Devvm
{
    public static class Job
    {
        const int ExtendedLimitInformation = 9;
        const int BasicProcessIdList = 3;
        const int ExtendedLimitInformationSize = 144;
        const int LimitFlagsOffset = 16;
        const int KillOnJobClose = 0x2000;
        const int ErrorMoreData = 234;

        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        static extern IntPtr CreateJobObjectW(IntPtr attributes, string name);

        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool SetInformationJobObject(IntPtr job, int infoClass, IntPtr info, int length);

        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);

        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool QueryInformationJobObject(IntPtr job, int infoClass, IntPtr info, int length, IntPtr returned);

        static IntPtr job = IntPtr.Zero;

        public static void EnterOwnJob()
        {
            job = CreateJobObjectW(IntPtr.Zero, null);
            if (job == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
            IntPtr info = Marshal.AllocHGlobal(ExtendedLimitInformationSize);
            try
            {
                for (int i = 0; i < ExtendedLimitInformationSize; i++) Marshal.WriteByte(info, i, 0);
                Marshal.WriteInt32(info, LimitFlagsOffset, KillOnJobClose);
                if (!SetInformationJobObject(job, ExtendedLimitInformation, info, ExtendedLimitInformationSize))
                    throw new Win32Exception(Marshal.GetLastWin32Error());
            }
            finally { Marshal.FreeHGlobal(info); }
            if (!AssignProcessToJobObject(job, Process.GetCurrentProcess().Handle))
                throw new Win32Exception(Marshal.GetLastWin32Error());
        }

        // The ids of the processes in the job: a header of two 32-bit counts, then one pointer-sized id per process.
        static int[] Members()
        {
            int capacity = 64;
            while (true)
            {
                int size = 8 + IntPtr.Size * capacity;
                IntPtr buffer = Marshal.AllocHGlobal(size);
                try
                {
                    bool ok = QueryInformationJobObject(job, BasicProcessIdList, buffer, size, IntPtr.Zero);
                    int assigned = Marshal.ReadInt32(buffer, 0);
                    if (ok)
                    {
                        int listed = Marshal.ReadInt32(buffer, 4);
                        int[] ids = new int[listed];
                        for (int i = 0; i < listed; i++) ids[i] = (int)Marshal.ReadIntPtr(buffer, 8 + IntPtr.Size * i);
                        return ids;
                    }
                    if (Marshal.GetLastWin32Error() != ErrorMoreData) throw new Win32Exception(Marshal.GetLastWin32Error());
                    capacity = Math.Max(capacity * 2, assigned + 64);
                }
                finally { Marshal.FreeHGlobal(buffer); }
            }
        }

        // Ends every process in the job but the caller, until none is left: a process that started while another was
        // being ended is in the next list.
        public static void KillOthers()
        {
            int self = Environment.ProcessId;
            while (true)
            {
                bool any = false;
                foreach (int id in Members())
                {
                    if (id == self) continue;
                    any = true;
                    try { using (Process p = Process.GetProcessById(id)) p.Kill(); }
                    catch (ArgumentException) { }
                    catch (InvalidOperationException) { }
                    catch (Win32Exception) { }
                }
                if (!any) return;
                Thread.Sleep(10);
            }
        }
    }
}
'@

# A request's caller has ended when its process is gone or the process of that id started at another time.
function Test-CallerEnded([string]$Dir) {
    try { $text = [IO.File]::ReadAllText((Join-Path $Dir $callerName)) } catch { return $true }
    if ($text -notmatch '^\s*(\d+)\s+(\d+)\s*$') { return $true }
    try {
        $process = [Diagnostics.Process]::GetProcessById([int]$Matches[1])
        try { return $process.StartTime.ToUniversalTime().Ticks -ne [long]$Matches[2] } finally { $process.Dispose() }
    } catch { return $true }
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
$child = $null
try {
    # Parsed again: the sweep above skipped it.
    $text = [IO.File]::ReadAllText((Join-Path $dir $callerName))
    if ($text -notmatch '^\s*(\d+)\s+(\d+)\s*$') { throw 'the request names no caller' }
    $callerPid = [int]$Matches[1]
    $callerExit = [Threading.Tasks.Task]::CompletedTask
    if (-not (Test-CallerEnded $dir)) {
        $caller = [Diagnostics.Process]::GetProcessById($callerPid)
        $callerExit = $caller.WaitForExitAsync()
    }
    if ($callerExit.IsCompleted) {
        $callerGone = $true
    } else {
        [Devvm.Job]::EnterOwnJob()
        # No buffering: the caller reads the files while they grow.
        $out = [IO.FileStream]::new((Join-Path $dir $stdoutName), 'Create', 'Write', 'Read', 1)
        $err = [IO.FileStream]::new((Join-Path $dir $stderrName), 'Create', 'Write', 'Read', 1)
        $psi = [Diagnostics.ProcessStartInfo]::new($pwsh)
        foreach ($argument in '-NoProfile', '-NonInteractive', '-File', (Join-Path $dir $scriptName)) { $psi.ArgumentList.Add($argument) }
        $psi.UseShellExecute = $false
        $psi.CreateNoWindow = $true
        $psi.RedirectStandardInput = $true
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $child = [Diagnostics.Process]::Start($psi)
        # devvm's exec sends no input: the command's standard input is at end of file.
        $child.StandardInput.Close()
        $finished = [Threading.Tasks.Task]::WhenAll([Threading.Tasks.Task[]]@(
            $child.StandardOutput.BaseStream.CopyToAsync($out),
            $child.StandardError.BaseStream.CopyToAsync($err),
            $child.WaitForExitAsync()))
        if ([Threading.Tasks.Task]::WaitAny([Threading.Tasks.Task[]]@($finished, $callerExit)) -ne 0) {
            $callerGone = $true
            [Devvm.Job]::KillOthers()
        }
        $finished.Wait()
        if (-not $callerGone) { $exitCode = $child.ExitCode }
    }
} catch {
    $failure = "devvm: the elevated runner failed: $($_.Exception.Message)"
    # No caller to answer: the request goes.
    if ($null -eq $callerExit) { $callerGone = $true }
    if ($null -ne $child) { try { [Devvm.Job]::KillOthers() } catch { } }
} finally {
    foreach ($stream in $out, $err) { if ($null -ne $stream) { $stream.Dispose() } }
}

if (-not $callerGone) {
    if ($failure) { [IO.File]::AppendAllText((Join-Path $dir $stderrName), $failure + "`n", $utf8) }
    # The status, then done: the caller reads the status only once done exists.
    [IO.File]::WriteAllText((Join-Path $dir $exitName), $exitCode.ToString([Globalization.CultureInfo]::InvariantCulture), $utf8)
    [IO.File]::WriteAllText((Join-Path $dir $doneName), '', $utf8)
    # The caller says when it has read everything; a caller that ends first is as good. Neither wait has a time bound.
    while (-not [IO.File]::Exists((Join-Path $dir $readName)) -and -not $callerExit.IsCompleted) { Start-Sleep -Milliseconds $pause }
}
# The last act: the request goes. The claim is closed first, or it would keep its own directory.
$claim.Dispose()
try { Remove-Item -LiteralPath $dir -Recurse -Force } catch { }
